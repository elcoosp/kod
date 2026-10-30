//! `FileInfoTool` — report metadata for a path (size, kind, timestamps).
//!
//! Extracted from `tools.rs`. Contiguous span: struct, `new()`
//! constructor, `Default` impl, `Tool` impl. Uses only `ToolContext`,
//! `ToolResult`, and kod-types enums — none of `tools.rs`'s private
//! helpers.

use serde_json::Value;

use crate::{Tool, ToolContext};
use kod_error::{KodError, Result};
use kod_types::{
    GitAccess, ToolCategory, ToolDefinition, ToolId, ToolPermissions, ToolResult, TrustLevel,
};

/// Get file information
pub struct FileInfoTool {
    pub definition: ToolDefinition,
}

impl FileInfoTool {
    pub fn new() -> Self {
        Self {
            definition: ToolDefinition {
                trust_level: kod_types::trust::TrustLevel::default(),
                id: ToolId::new(),
                name: "file_info".to_string(),
                description: "Get information about a file".to_string(),
                category: ToolCategory::FileSystem,
                parameters_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path to the file"
                        }
                    },
                    "required": ["path"]
                }),
                permissions: ToolPermissions {
                    read_files: true,
                    write_files: false,
                    execute_commands: false,
                    network_access: false,
                    git_access: kod_types::GitAccess::None,
                    allowed_paths: Vec::new(),
                    forbidden_paths: Vec::new(),
                },
                load_mode: Default::default(),
            },
        }
    }
}

impl Default for FileInfoTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Tool for FileInfoTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    async fn execute(&self, params: &Value, context: &ToolContext) -> Result<ToolResult> {
        let path = params["path"]
            .as_str()
            .ok_or_else(|| KodError::InvalidParameters {
                reason: "Missing 'path' parameter".to_string(),
            })?;

        let resolved = context.resolve_path(path)?;
        context.can_read(&resolved)?;

        let metadata = std::fs::metadata(&resolved).map_err(KodError::Io)?;

        Ok(ToolResult::Success(serde_json::json!({
            "path": resolved.to_string_lossy().to_string(),
            "size": metadata.len(),
            "is_file": metadata.is_file(),
            "is_dir": metadata.is_dir(),
            "modified": metadata.modified().map(|t| t.duration_since(std::time::SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)).unwrap_or(0),
        })))
    }
}
