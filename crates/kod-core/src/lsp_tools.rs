//! LSP tools exposed to the model (D5-L3a).
//!
//! Four read-only tools that reach the engine's shared LSP client:
//!
//! - `lsp_diagnostics(path)` — errors and warnings for one file.
//! - `lsp_definition(path, line, column)` — where a symbol is defined.
//! - `lsp_references(path, line, column)` — every mention of a symbol.
//! - `lsp_hover(path, line, column)` — the server's type/doc summary.
//!
//! Coordinates are 1-based in the tool JSON (matching `grep`,
//! compilers, editors). The engine's methods pass them through to
//! `LspClient`, which converts to LSP's 0-based wire form.
//!
//! Every tool returns a structured `ToolResult::Error` when no
//! server is available for the file's language, so the model sees an
//! honest "not available" rather than an empty result it might read
//! as "no errors".

use kod_tools::{Tool, ToolContext};
use kod_error::{KodError, Result};
use kod_types::{ToolCategory, ToolDefinition, ToolId, ToolPermissions, ToolResult};
use serde_json::Value;
use std::sync::Arc;

/// Read a file's contents for the diagnostics tool. A missing file is
/// a tool-level error the model can act on.
fn read_file(path: &std::path::Path) -> std::result::Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))
}

/// The shared `ToolDefinition` shape for the LSP tools — same
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
    slot: Arc<tokio::sync::Mutex<Option<kod_lsp::LspClient>>>,
    working_dir: std::path::PathBuf,
}

impl LspDiagnosticsTool {
    pub fn new(
        slot: Arc<tokio::sync::Mutex<Option<kod_lsp::LspClient>>>,
        working_dir: std::path::PathBuf,
    ) -> Self {
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
            slot,
            working_dir,
        }
    }
}

#[async_trait::async_trait]
impl Tool for LspDiagnosticsTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    async fn execute(&self, params: &Value, context: &ToolContext) -> Result<ToolResult> {
        let path_arg = params["path"].as_str().ok_or_else(|| {
            KodError::InvalidParameters {
                reason: "lsp_diagnostics: 'path' is required".to_string(),
            }
        })?;
        let path = context.resolve_path(path_arg)?;
        context.can_read(&path)?;
        if crate::engine::KodEngine::lsp_binary_for(&path).is_none() {
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
        if let Err(e) = ensure_started(&self.slot, &self.working_dir, &path).await {
            return Ok(ToolResult::Error(e));
        }
        let diags = {
            let mut guard = self.slot.lock().await;
            let client = guard.as_mut().expect("ensure_started set Some");
            client
                .diagnostics(
                    &path,
                    &content,
                    std::time::Duration::from_secs(30),
                )
                .await
                .unwrap_or_default()
        };
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
    slot: Arc<tokio::sync::Mutex<Option<kod_lsp::LspClient>>>,
    working_dir: std::path::PathBuf,
}

impl LspDefinitionTool {
    pub fn new(
        slot: Arc<tokio::sync::Mutex<Option<kod_lsp::LspClient>>>,
        working_dir: std::path::PathBuf,
    ) -> Self {
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
            slot,
            working_dir,
        }
    }
}

#[async_trait::async_trait]
impl Tool for LspDefinitionTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    async fn execute(&self, params: &Value, context: &ToolContext) -> Result<ToolResult> {
        let path_arg = params["path"].as_str().ok_or_else(|| {
            KodError::InvalidParameters {
                reason: "lsp_definition: 'path' is required".to_string(),
            }
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
        if crate::engine::KodEngine::lsp_binary_for(&path).is_none() {
            return Ok(ToolResult::Error(format!(
                "no language server for {}",
                path.display()
            )));
        }
        if let Err(e) = ensure_started(&self.slot, &self.working_dir, &path).await {
            return Ok(ToolResult::Error(e));
        }
        let locations = {
            let mut guard = self.slot.lock().await;
            let client = guard.as_mut().expect("ensure_started set Some");
            let pos = kod_lsp::Position { line, column };
            client.definition(&path, pos).await.unwrap_or_default()
        };
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
    slot: Arc<tokio::sync::Mutex<Option<kod_lsp::LspClient>>>,
    working_dir: std::path::PathBuf,
}

impl LspReferencesTool {
    pub fn new(
        slot: Arc<tokio::sync::Mutex<Option<kod_lsp::LspClient>>>,
        working_dir: std::path::PathBuf,
    ) -> Self {
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
            slot,
            working_dir,
        }
    }
}

#[async_trait::async_trait]
impl Tool for LspReferencesTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    async fn execute(&self, params: &Value, context: &ToolContext) -> Result<ToolResult> {
        let path_arg = params["path"].as_str().ok_or_else(|| {
            KodError::InvalidParameters {
                reason: "lsp_references: 'path' is required".to_string(),
            }
        })?;
        let line = params["line"].as_u64().unwrap_or(0) as u32;
        let column = params["column"].as_u64().unwrap_or(0) as u32;
        if line == 0 || column == 0 {
            return Ok(ToolResult::Error(
                "lsp_references: line and column are 1-based and must be ≥ 1".to_string(),
            ));
        }
        let include_declaration = params["include_declaration"]
            .as_bool()
            .unwrap_or(true);
        let path = context.resolve_path(path_arg)?;
        context.can_read(&path)?;
        if crate::engine::KodEngine::lsp_binary_for(&path).is_none() {
            return Ok(ToolResult::Error(format!(
                "no language server for {}",
                path.display()
            )));
        }
        if let Err(e) = ensure_started(&self.slot, &self.working_dir, &path).await {
            return Ok(ToolResult::Error(e));
        }
        let locations = {
            let mut guard = self.slot.lock().await;
            let client = guard.as_mut().expect("ensure_started set Some");
            let pos = kod_lsp::Position { line, column };
            client
                .references(&path, pos, include_declaration)
                .await
                .unwrap_or_default()
        };
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
    slot: Arc<tokio::sync::Mutex<Option<kod_lsp::LspClient>>>,
    working_dir: std::path::PathBuf,
}

impl LspHoverTool {
    pub fn new(
        slot: Arc<tokio::sync::Mutex<Option<kod_lsp::LspClient>>>,
        working_dir: std::path::PathBuf,
    ) -> Self {
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
            slot,
            working_dir,
        }
    }
}

#[async_trait::async_trait]
impl Tool for LspHoverTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    async fn execute(&self, params: &Value, context: &ToolContext) -> Result<ToolResult> {
        let path_arg = params["path"].as_str().ok_or_else(|| {
            KodError::InvalidParameters {
                reason: "lsp_hover: 'path' is required".to_string(),
            }
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
        if crate::engine::KodEngine::lsp_binary_for(&path).is_none() {
            return Ok(ToolResult::Error(format!(
                "no language server for {}",
                path.display()
            )));
        }
        if let Err(e) = ensure_started(&self.slot, &self.working_dir, &path).await {
            return Ok(ToolResult::Error(e));
        }
        let hover = {
            let mut guard = self.slot.lock().await;
            let client = guard.as_mut().expect("ensure_started set Some");
            let pos = kod_lsp::Position { line, column };
            client.hover(&path, pos).await.ok()
        };
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

/// Start the LSP client if none is running and the file's language has
/// a server binary. The slot is shared with the engine, so the client
/// started here is what `kod doctor` and any auto-diagnostics hook
/// also see.
async fn ensure_started(
    slot: &Arc<tokio::sync::Mutex<Option<kod_lsp::LspClient>>>,
    working_dir: &std::path::Path,
    path: &std::path::Path,
) -> std::result::Result<(), String> {
    let mut guard = slot.lock().await;
    if guard.is_some() {
        return Ok(());
    }
    let Some(binary) = crate::engine::KodEngine::lsp_binary_for(path) else {
        return Err(format!("no language server for {}", path.display()));
    };
    let mut client = kod_lsp::LspClient::start(binary, working_dir)
        .await
        .map_err(|e| e.to_string())?;
    client
        .initialize()
        .await
        .map_err(|e| e.to_string())?;
    *guard = Some(client);
    Ok(())
}

