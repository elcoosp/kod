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
