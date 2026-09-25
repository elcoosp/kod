//! LSP tools exposed to the model (design D5-L3).
//!
//! Four read-only tools that reach the engine's shared LSP pool
//! (`kod_lsp::LspManager`): one client per language, lazily started on
//! the first request for its language. The manager is the same one the
//! engine uses for the post-write diagnostics hook (D5.2), so a server
//! started by the hook is reused by the tool and vice versa.
//!
//! Coordinates are 1-based in the tool JSON (matching `grep`,
//! compilers, editors). The manager's methods take `kod_lsp::Position`
//! and the client converts to LSP's 0-based wire form internally.
//!
//! Every tool returns a structured `ToolResult::Error` when no server
//! is available for the file's language, so the model sees an honest
//! "not available" rather than an empty result it might read as "no
//! errors".

use kod_error::{KodError, Result};
use kod_tools::{Tool, ToolContext};
use kod_types::{ToolCategory, ToolDefinition, ToolId, ToolPermissions, ToolResult};
use serde_json::Value;
use std::sync::Arc;

/// Read a file's contents. A missing file is a tool-level error the
/// model can act on.
fn read_file(path: &std::path::Path) -> std::result::Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))
}

/// The shared `ToolPermissions` shape for the LSP tools — same
/// permissions, same category. The name, description, and schema are
/// per-tool.
fn lsp_permissions() -> ToolPermissions {
    ToolPermissions {
        read_files: true,
        write_files: false,
        execute_commands: false,
        network_access: false,
        git_access: kod_types::GitAccess::None,
        allowed_paths: Vec::new(),
        forbidden_paths: Vec::new(),
    }
}

/// `lsp_diagnostics(path)`.
pub struct LspDiagnosticsTool {
    pub definition: ToolDefinition,
    manager: Arc<kod_lsp::LspManager>,
}

impl LspDiagnosticsTool {
    pub fn new(manager: Arc<kod_lsp::LspManager>) -> Self {
        Self {
            definition: ToolDefinition {
                trust_level: kod_types::trust::TrustLevel::default(),
                id: ToolId::new(),
                name: "lsp_diagnostics".to_string(),
                description: "Ask the language server for diagnostics on one file. \
                    Returns an array of {file, line, column, severity, code, message}. \
                    Faster than `check` (which compiles the whole project) and useful \
                    right after an edit."
                    .to_string(),
                category: ToolCategory::Code,
                parameters_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "File to check" }
                    },
                    "required": ["path"],
                    "additionalProperties": false
                }),
                permissions: lsp_permissions(),
                load_mode: Default::default(),
            },
            manager,
        }
    }
}

#[async_trait::async_trait]
impl Tool for LspDiagnosticsTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    async fn execute(&self, params: &Value, context: &ToolContext) -> Result<ToolResult> {
        let path_arg = params["path"]
            .as_str()
            .ok_or_else(|| KodError::InvalidParameters {
                reason: "lsp_diagnostics: 'path' is required".to_string(),
            })?;
        let path = context.resolve_path(path_arg)?;
        context.can_read(&path)?;
        if kod_lsp::binary_for_path(&path).is_none() {
            return Ok(ToolResult::Error(format!(
                "no language server for {} — install rust-analyzer / \
                 pyright-langserver / typescript-language-server / gopls",
                path.display()
            )));
        }
        let content = match read_file(&path) {
            Ok(c) => c,
            Err(e) => return Ok(ToolResult::Error(e)),
        };
        let diags = self
            .manager
            .diagnostics(&path, &content, std::time::Duration::from_secs(30))
            .await;
        let arr: Vec<Value> = diags
            .iter()
            .map(|d| {
                serde_json::json!({
                    "file": d.file,
                    "line": d.line,
                    "column": d.column,
                    "severity": d.severity,
                    "code": d.code,
                    "message": d.message,
                })
            })
            .collect();
        Ok(ToolResult::Success(serde_json::json!({
            "count": arr.len(),
            "diagnostics": arr,
        })))
    }
}

/// `lsp_definition(path, line, column)`.
pub struct LspDefinitionTool {
    pub definition: ToolDefinition,
    manager: Arc<kod_lsp::LspManager>,
}

impl LspDefinitionTool {
    pub fn new(manager: Arc<kod_lsp::LspManager>) -> Self {
        Self {
            definition: ToolDefinition {
                trust_level: kod_types::trust::TrustLevel::default(),
                id: ToolId::new(),
                name: "lsp_definition".to_string(),
                description: "Ask the language server where the symbol at a given \
                    position is defined. Returns an array of {file, line, column}. \
                    Much more precise than grep for symbol navigation."
                    .to_string(),
                category: ToolCategory::Code,
                parameters_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "line": { "type": "integer", "description": "1-based line" },
                        "column": { "type": "integer", "description": "1-based column" }
                    },
                    "required": ["path", "line", "column"],
                    "additionalProperties": false
                }),
                permissions: lsp_permissions(),
                load_mode: Default::default(),
            },
            manager,
        }
    }
}

#[async_trait::async_trait]
impl Tool for LspDefinitionTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    async fn execute(&self, params: &Value, context: &ToolContext) -> Result<ToolResult> {
        let path_arg = params["path"]
            .as_str()
            .ok_or_else(|| KodError::InvalidParameters {
                reason: "lsp_definition: 'path' is required".to_string(),
            })?;
        let line = params["line"].as_u64().unwrap_or(0) as u32;
        let column = params["column"].as_u64().unwrap_or(0) as u32;
        if line == 0 || column == 0 {
            return Ok(ToolResult::Error(
                "lsp_definition: line and column are 1-based and must be ≥ 1".to_string(),
            ));
        }
        let path = context.resolve_path(path_arg)?;
        context.can_read(&path)?;
        if kod_lsp::binary_for_path(&path).is_none() {
            return Ok(ToolResult::Error(format!(
                "no language server for {}",
                path.display()
            )));
        }
        let locations = self
            .manager
            .definition(&path, kod_lsp::Position { line, column })
            .await;
        let arr: Vec<Value> = locations
            .iter()
            .map(|loc| {
                serde_json::json!({
                    "file": loc.file,
                    "line": loc.range.start.line,
                    "column": loc.range.start.column,
                })
            })
            .collect();
        Ok(ToolResult::Success(serde_json::json!({
            "count": arr.len(),
            "definitions": arr,
        })))
    }
}

/// `lsp_references(path, line, column, include_declaration?)`.
pub struct LspReferencesTool {
    pub definition: ToolDefinition,
    manager: Arc<kod_lsp::LspManager>,
}

impl LspReferencesTool {
    pub fn new(manager: Arc<kod_lsp::LspManager>) -> Self {
        Self {
            definition: ToolDefinition {
                trust_level: kod_types::trust::TrustLevel::default(),
                id: ToolId::new(),
                name: "lsp_references".to_string(),
                description: "Ask the language server for every reference to the \
                    symbol at a given position. Returns {file, line, column} for each. \
                    Use it to answer 'where is this used?' without grep's \
                    false positives."
                    .to_string(),
                category: ToolCategory::Code,
                parameters_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "line": { "type": "integer", "description": "1-based line" },
                        "column": { "type": "integer", "description": "1-based column" },
                        "include_declaration": {
                            "type": "boolean",
                            "description": "When true (default), the symbol's own definition is included."
                        }
                    },
                    "required": ["path", "line", "column"],
                    "additionalProperties": false
                }),
                permissions: lsp_permissions(),
                load_mode: Default::default(),
            },
            manager,
        }
    }
}

#[async_trait::async_trait]
impl Tool for LspReferencesTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    async fn execute(&self, params: &Value, context: &ToolContext) -> Result<ToolResult> {
        let path_arg = params["path"]
            .as_str()
            .ok_or_else(|| KodError::InvalidParameters {
                reason: "lsp_references: 'path' is required".to_string(),
            })?;
        let line = params["line"].as_u64().unwrap_or(0) as u32;
        let column = params["column"].as_u64().unwrap_or(0) as u32;
        if line == 0 || column == 0 {
            return Ok(ToolResult::Error(
                "lsp_references: line and column are 1-based and must be ≥ 1".to_string(),
            ));
        }
        let include_declaration = params["include_declaration"].as_bool().unwrap_or(true);
        let path = context.resolve_path(path_arg)?;
        context.can_read(&path)?;
        if kod_lsp::binary_for_path(&path).is_none() {
            return Ok(ToolResult::Error(format!(
                "no language server for {}",
                path.display()
            )));
        }
        let locations = self
            .manager
            .references(
                &path,
                kod_lsp::Position { line, column },
                include_declaration,
            )
            .await;
        let arr: Vec<Value> = locations
            .iter()
            .map(|loc| {
                serde_json::json!({
                    "file": loc.file,
                    "line": loc.range.start.line,
                    "column": loc.range.start.column,
                })
            })
            .collect();
        Ok(ToolResult::Success(serde_json::json!({
            "count": arr.len(),
            "references": arr,
        })))
    }
}

/// `lsp_hover(path, line, column)`.
pub struct LspHoverTool {
    pub definition: ToolDefinition,
    manager: Arc<kod_lsp::LspManager>,
}

impl LspHoverTool {
    pub fn new(manager: Arc<kod_lsp::LspManager>) -> Self {
        Self {
            definition: ToolDefinition {
                trust_level: kod_types::trust::TrustLevel::default(),
                id: ToolId::new(),
                name: "lsp_hover".to_string(),
                description: "Ask the language server for its summary of the symbol \
                    at a given position (type, signature, doc comment). Faster than \
                    reading the source when you just want to know the type."
                    .to_string(),
                category: ToolCategory::Code,
                parameters_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "line": { "type": "integer", "description": "1-based line" },
                        "column": { "type": "integer", "description": "1-based column" }
                    },
                    "required": ["path", "line", "column"],
                    "additionalProperties": false
                }),
                permissions: lsp_permissions(),
                load_mode: Default::default(),
            },
            manager,
        }
    }
}

#[async_trait::async_trait]
impl Tool for LspHoverTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    async fn execute(&self, params: &Value, context: &ToolContext) -> Result<ToolResult> {
        let path_arg = params["path"]
            .as_str()
            .ok_or_else(|| KodError::InvalidParameters {
                reason: "lsp_hover: 'path' is required".to_string(),
            })?;
        let line = params["line"].as_u64().unwrap_or(0) as u32;
        let column = params["column"].as_u64().unwrap_or(0) as u32;
        if line == 0 || column == 0 {
            return Ok(ToolResult::Error(
                "lsp_hover: line and column are 1-based and must be ≥ 1".to_string(),
            ));
        }
        let path = context.resolve_path(path_arg)?;
        context.can_read(&path)?;
        if kod_lsp::binary_for_path(&path).is_none() {
            return Ok(ToolResult::Error(format!(
                "no language server for {}",
                path.display()
            )));
        }
        let hover = self
            .manager
            .hover(&path, kod_lsp::Position { line, column })
            .await;
        match hover {
            Some(h) => Ok(ToolResult::Success(serde_json::json!({
                "text": h.text,
            }))),
            None => Ok(ToolResult::Success(serde_json::json!({
                "text": "",
                "note": "server returned no hover information",
            }))),
        }
    }
}

/// Coverage for the four LSP tool wrappers. Every test targets a
/// branch that returns **before** any language-server subprocess is
/// spawned: parameter validation, path resolution, read permission,
/// and the "no server for this extension" guard. The branch that
/// actually talks to a server is exercised end-to-end by the LSP
/// integration tests, not here.
#[cfg(test)]
mod coverage_lsp_tools {
    use super::*;
    use kod_types::GitAccess;
    use serde_json::json;
    use tempfile::TempDir;

    fn manager() -> Arc<kod_lsp::LspManager> {
        Arc::new(kod_lsp::LspManager::new(std::env::temp_dir()))
    }

    fn context_with_read(root: &std::path::Path) -> ToolContext {
        let perms = ToolPermissions {
            read_files: true,
            write_files: false,
            execute_commands: false,
            network_access: false,
            git_access: GitAccess::None,
            allowed_paths: Vec::new(),
            forbidden_paths: Vec::new(),
        };
        ToolContext::new(root).with_permissions(perms)
    }

    fn context_without_read(root: &std::path::Path) -> ToolContext {
        ToolContext::new(root)
    }

    // ---- lsp_permissions ----------------------------------------------

    #[test]
    fn lsp_permissions_are_read_only() {
        // The whole LSP tool family shares one permission shape. A
        // regression that flipped `write_files` to true would let the
        // model mutate the workspace through a tool named "diagnostics".
        let p = lsp_permissions();
        assert!(p.read_files);
        assert!(!p.write_files);
        assert!(!p.execute_commands);
        assert!(!p.network_access);
        assert_eq!(p.git_access, GitAccess::None);
    }

    // ---- LspDiagnosticsTool -------------------------------------------

    #[tokio::test]
    async fn diagnostics_missing_path_is_an_invalid_parameter() {
        let tool = LspDiagnosticsTool::new(manager());
        let ctx = context_with_read(&std::env::temp_dir());
        let err = tool.execute(&json!({}), &ctx).await.unwrap_err();
        assert!(
            err.to_string().contains("'path' is required"),
            "error must name the missing parameter: {err}",
        );
    }

    #[tokio::test]
    async fn diagnostics_path_wrong_type_is_an_invalid_parameter() {
        let tool = LspDiagnosticsTool::new(manager());
        let ctx = context_with_read(&std::env::temp_dir());
        let err = tool.execute(&json!({"path": 42}), &ctx).await.unwrap_err();
        assert!(err.to_string().contains("'path' is required"));
    }

    #[tokio::test]
    async fn diagnostics_path_outside_the_workspace_is_denied() {
        let tmp = TempDir::new().unwrap();
        let tool = LspDiagnosticsTool::new(manager());
        let ctx = context_with_read(tmp.path());
        let err = tool
            .execute(&json!({"path": "/etc/hosts"}), &ctx)
            .await
            .unwrap_err();
        // resolve_path refuses anything that escapes the working
        // directory; the error names the action.
        let msg = err.to_string();
        assert!(
            msg.contains("resolve path") || msg.contains("Permission") || msg.contains("denied"),
            "unexpected error: {msg}",
        );
    }

    #[tokio::test]
    async fn diagnostics_without_read_permission_is_denied() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("x.rs");
        std::fs::write(&file, "fn main() {}").unwrap();
        let tool = LspDiagnosticsTool::new(manager());
        let ctx = context_without_read(tmp.path());
        let err = tool
            .execute(&json!({"path": "x.rs"}), &ctx)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("read") || msg.contains("permission") || msg.contains("denied"),
            "expected a read-permission error, got: {msg}",
        );
    }

    #[tokio::test]
    async fn diagnostics_unknown_extension_reports_no_server() {
        // `.xyz` maps to `plaintext`, which has no server binary.
        // The tool returns `ToolResult::Error`, not `Err` — the
        // model must see an honest "not available" rather than an
        // empty diagnostics array it might read as "no problems".
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("mystery.xyz");
        std::fs::write(&file, "some content").unwrap();
        let tool = LspDiagnosticsTool::new(manager());
        let ctx = context_with_read(tmp.path());
        let result = tool
            .execute(&json!({"path": "mystery.xyz"}), &ctx)
            .await
            .unwrap();
        match result {
            ToolResult::Error(msg) => assert!(
                msg.contains("no language server"),
                "error must name the missing server: {msg}",
            ),
            other => panic!("expected ToolResult::Error, got {other:?}"),
        }
    }

    // ---- LspDefinitionTool --------------------------------------------

    #[tokio::test]
    async fn definition_missing_path_is_an_invalid_parameter() {
        let tool = LspDefinitionTool::new(manager());
        let ctx = context_with_read(&std::env::temp_dir());
        let err = tool
            .execute(&json!({"line": 1, "column": 1}), &ctx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("'path' is required"));
    }

    #[tokio::test]
    async fn definition_zero_line_reports_an_error() {
        // 1-based coordinates: line 0 is not valid. The tool returns
        // `Ok(Error)` rather than `Err` — an invalid position is a
        // user-model mistake, not a transport failure.
        let tool = LspDefinitionTool::new(manager());
        let ctx = context_with_read(&std::env::temp_dir());
        let result = tool
            .execute(&json!({"path": "x.rs", "line": 0, "column": 5}), &ctx)
            .await
            .unwrap();
        match result {
            ToolResult::Error(msg) => assert!(
                msg.contains("1-based") || msg.contains("≥ 1"),
                "error must explain the 1-based convention: {msg}",
            ),
            other => panic!("expected ToolResult::Error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn definition_zero_column_reports_an_error() {
        let tool = LspDefinitionTool::new(manager());
        let ctx = context_with_read(&std::env::temp_dir());
        let result = tool
            .execute(&json!({"path": "x.rs", "line": 5, "column": 0}), &ctx)
            .await
            .unwrap();
        assert!(matches!(result, ToolResult::Error(_)));
    }

    #[tokio::test]
    async fn definition_missing_line_and_column_both_default_to_zero() {
        // `unwrap_or(0)` on a missing line/column means the 1-based
        // check fires. A regression that defaulted to 1 instead
        // would silently call the server with position (1, 1).
        let tool = LspDefinitionTool::new(manager());
        let ctx = context_with_read(&std::env::temp_dir());
        let result = tool.execute(&json!({"path": "x.rs"}), &ctx).await.unwrap();
        assert!(matches!(result, ToolResult::Error(_)));
    }

    #[tokio::test]
    async fn definition_unknown_extension_reports_no_server() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("mystery.xyz"), "x").unwrap();
        let tool = LspDefinitionTool::new(manager());
        let ctx = context_with_read(tmp.path());
        let result = tool
            .execute(
                &json!({"path": "mystery.xyz", "line": 1, "column": 1}),
                &ctx,
            )
            .await
            .unwrap();
        match result {
            ToolResult::Error(msg) => assert!(msg.contains("no language server")),
            other => panic!("expected ToolResult::Error, got {other:?}"),
        }
    }

    // ---- LspReferencesTool --------------------------------------------

    #[tokio::test]
    async fn references_missing_path_is_an_invalid_parameter() {
        let tool = LspReferencesTool::new(manager());
        let ctx = context_with_read(&std::env::temp_dir());
        let err = tool
            .execute(&json!({"line": 1, "column": 1}), &ctx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("'path' is required"));
    }

    #[tokio::test]
    async fn references_zero_line_reports_an_error() {
        let tool = LspReferencesTool::new(manager());
        let ctx = context_with_read(&std::env::temp_dir());
        let result = tool
            .execute(&json!({"path": "x.rs", "line": 0, "column": 1}), &ctx)
            .await
            .unwrap();
        assert!(matches!(result, ToolResult::Error(_)));
    }

    #[tokio::test]
    async fn references_unknown_extension_reports_no_server() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("mystery.xyz"), "x").unwrap();
        let tool = LspReferencesTool::new(manager());
        let ctx = context_with_read(tmp.path());
        let result = tool
            .execute(
                &json!({"path": "mystery.xyz", "line": 1, "column": 1}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(matches!(result, ToolResult::Error(_)));
    }

    // ---- LspHoverTool -------------------------------------------------

    #[tokio::test]
    async fn hover_missing_path_is_an_invalid_parameter() {
        let tool = LspHoverTool::new(manager());
        let ctx = context_with_read(&std::env::temp_dir());
        let err = tool
            .execute(&json!({"line": 1, "column": 1}), &ctx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("'path' is required"));
    }

    #[tokio::test]
    async fn hover_zero_line_reports_an_error() {
        let tool = LspHoverTool::new(manager());
        let ctx = context_with_read(&std::env::temp_dir());
        let result = tool
            .execute(&json!({"path": "x.rs", "line": 0, "column": 1}), &ctx)
            .await
            .unwrap();
        assert!(matches!(result, ToolResult::Error(_)));
    }

    #[tokio::test]
    async fn hover_unknown_extension_reports_no_server() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("mystery.xyz"), "x").unwrap();
        let tool = LspHoverTool::new(manager());
        let ctx = context_with_read(tmp.path());
        let result = tool
            .execute(
                &json!({"path": "mystery.xyz", "line": 1, "column": 1}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(matches!(result, ToolResult::Error(_)));
    }

    // ---- definitions: name, category, schema shape --------------------

    #[test]
    fn every_lsp_tool_has_the_same_category_and_read_only_permissions() {
        // Four tools, one permissions shape, one category. A
        // regression that changed one tool's permission bit
        // independently would be visible here.
        let mgr = manager();
        let tools: Vec<Box<dyn Tool>> = vec![
            Box::new(LspDiagnosticsTool::new(mgr.clone())),
            Box::new(LspDefinitionTool::new(mgr.clone())),
            Box::new(LspReferencesTool::new(mgr.clone())),
            Box::new(LspHoverTool::new(mgr)),
        ];
        for tool in tools {
            let def = tool.definition();
            assert_eq!(def.category, ToolCategory::Code, "{}", def.name);
            assert!(def.permissions.read_files, "{}", def.name);
            assert!(!def.permissions.write_files, "{}", def.name);
            assert!(
                def.name.starts_with("lsp_"),
                "every tool in this module is namespaced under lsp_: {}",
                def.name,
            );
            assert_eq!(
                def.parameters_schema["type"], "object",
                "{}: schema must be an object",
                def.name,
            );
            assert!(
                def.parameters_schema["required"]
                    .as_array()
                    .map(|a| a.iter().any(|v| v == "path"))
                    .unwrap_or(false),
                "{}: 'path' must be required",
                def.name,
            );
            assert_eq!(
                def.parameters_schema["additionalProperties"], false,
                "{}: schema must reject unknown properties",
                def.name,
            );
        }
    }

    #[test]
    fn definition_and_hover_and_references_require_line_and_column() {
        let mgr = manager();
        for tool in [
            Box::new(LspDefinitionTool::new(mgr.clone())) as Box<dyn Tool>,
            Box::new(LspReferencesTool::new(mgr.clone())),
            Box::new(LspHoverTool::new(mgr)),
        ] {
            let def = tool.definition();
            let required: Vec<&str> = def.parameters_schema["required"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect();
            assert!(required.contains(&"path"), "{}", def.name);
            assert!(required.contains(&"line"), "{}", def.name);
            assert!(required.contains(&"column"), "{}", def.name);
        }
    }

    #[test]
    fn diagnostics_requires_only_path() {
        let def = LspDiagnosticsTool::new(manager()).definition();
        let required: Vec<&str> = def.parameters_schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(required, vec!["path"]);
    }

    #[test]
    fn references_schema_accepts_include_declaration() {
        let def = LspReferencesTool::new(manager()).definition();
        let props = &def.parameters_schema["properties"];
        assert!(
            props.get("include_declaration").is_some(),
            "references must accept the include_declaration flag",
        );
    }

    #[test]
    fn each_tool_gets_a_fresh_tool_id() {
        // `ToolId::new()` at construction means two instances of the
        // same tool have distinct ids. The engine registers the four
        // tools once, but a test or a future reload might construct
        // a second set; the ids must not collide.
        let mgr = manager();
        let a = LspDiagnosticsTool::new(mgr.clone()).definition().id;
        let b = LspDiagnosticsTool::new(mgr).definition().id;
        assert_ne!(a, b, "each construction must mint a fresh id");
    }
}
