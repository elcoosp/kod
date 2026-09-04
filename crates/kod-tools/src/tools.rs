//! Built-in tools for common agent operations.

use crate::{Tool, ToolContext};
use kod_error::{KodError, Result};
use kod_types::{ToolCategory, ToolDefinition, ToolId, ToolPermissions, ToolResult};
use serde_json::Value;

/// Read a file's contents
pub struct ReadFileTool {
    pub definition: ToolDefinition,
}

impl ReadFileTool {
    pub fn new() -> Self {
        Self {
            definition: ToolDefinition {
                id: ToolId::new(),
                name: "read_file".to_string(),
                description: "Read a file and return its contents".to_string(),
                category: ToolCategory::FileSystem,
                parameters_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path to the file to read"
                        }
                    },
                    "required": ["path"]
                }),
                permissions: ToolPermissions {
                    read_files: true,
                    write_files: false,
                    execute_commands: false,
                    network_access: false,
                    git_operations: false,
                    allowed_paths: Vec::new(),
                    forbidden_paths: Vec::new(),
                },
            },
        }
    }
}

impl Default for ReadFileTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Tool for ReadFileTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    async fn execute(&self, params: &Value, context: &ToolContext) -> Result<ToolResult> {
        let path = params["path"]
            .as_str()
            .ok_or_else(|| KodError::InvalidParameters {
                reason: "Missing 'path' parameter".to_string(),
            })?;

        let resolved = context.resolve_path(path);
        context.can_read(&resolved)?;

        let content = std::fs::read_to_string(&resolved).map_err(KodError::Io)?;

        Ok(ToolResult::Success(serde_json::json!({
            "path": resolved.to_string_lossy().to_string(),
            "content": content,
        })))
    }
}

/// Write content to a file
pub struct WriteFileTool {
    pub definition: ToolDefinition,
}

impl WriteFileTool {
    pub fn new() -> Self {
        Self {
            definition: ToolDefinition {
                id: ToolId::new(),
                name: "write_file".to_string(),
                description: "Write content to a file".to_string(),
                category: ToolCategory::FileSystem,
                parameters_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path to the file"
                        },
                        "content": {
                            "type": "string",
                            "description": "Content to write"
                        },
                        "append": {
                            "type": "boolean",
                            "description": "Append to file instead of overwriting"
                        }
                    },
                    "required": ["path", "content"]
                }),
                permissions: ToolPermissions {
                    read_files: false,
                    write_files: true,
                    execute_commands: false,
                    network_access: false,
                    git_operations: false,
                    allowed_paths: Vec::new(),
                    forbidden_paths: Vec::new(),
                },
            },
        }
    }
}

impl Default for WriteFileTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Tool for WriteFileTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    async fn execute(&self, params: &Value, context: &ToolContext) -> Result<ToolResult> {
        let path = params["path"]
            .as_str()
            .ok_or_else(|| KodError::InvalidParameters {
                reason: "Missing 'path' parameter".to_string(),
            })?;
        let content = params["content"]
            .as_str()
            .ok_or_else(|| KodError::InvalidParameters {
                reason: "Missing 'content' parameter".to_string(),
            })?;
        let append = params["append"].as_bool().unwrap_or(false);

        let resolved = context.resolve_path(path);
        context.can_write(&resolved)?;

        if append {
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&resolved)
                .map_err(KodError::Io)?;
            use std::io::Write;
            write!(file, "{}", content).map_err(KodError::Io)?;
        } else {
            std::fs::write(&resolved, content).map_err(KodError::Io)?;
        }

        Ok(ToolResult::Success(serde_json::json!({
            "path": resolved.to_string_lossy().to_string(),
            "written": content.len(),
        })))
    }
}

/// Execute a shell command
pub struct ExecuteCommandTool {
    pub definition: ToolDefinition,
}

impl ExecuteCommandTool {
    pub fn new() -> Self {
        Self {
            definition: ToolDefinition {
                id: ToolId::new(),
                name: "execute_command".to_string(),
                description: "Execute a shell command".to_string(),
                category: ToolCategory::System,
                parameters_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "command": {
                            "type": "string",
                            "description": "Command to execute"
                        }
                    },
                    "required": ["command"]
                }),
                permissions: ToolPermissions {
                    read_files: false,
                    write_files: false,
                    execute_commands: true,
                    network_access: false,
                    git_operations: false,
                    allowed_paths: Vec::new(),
                    forbidden_paths: Vec::new(),
                },
            },
        }
    }
}

impl Default for ExecuteCommandTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Tool for ExecuteCommandTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    async fn execute(&self, params: &Value, context: &ToolContext) -> Result<ToolResult> {
        let command = params["command"]
            .as_str()
            .ok_or_else(|| KodError::InvalidParameters {
                reason: "Missing 'command' parameter".to_string(),
            })?;

        context.can_execute_command(command)?;

        let output = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .output()
            .await
            .map_err(KodError::Io)?;

        Ok(ToolResult::Success(serde_json::json!({
            "stdout": String::from_utf8_lossy(&output.stdout).to_string(),
            "stderr": String::from_utf8_lossy(&output.stderr).to_string(),
            "exit_code": output.status.code().unwrap_or(-1),
        })))
    }
}

/// List tools available in a directory for filesystem operations
pub struct ListFilesTool {
    pub definition: ToolDefinition,
}

impl ListFilesTool {
    pub fn new() -> Self {
        Self {
            definition: ToolDefinition {
                id: ToolId::new(),
                name: "list_files".to_string(),
                description: "List files in a directory".to_string(),
                category: ToolCategory::FileSystem,
                parameters_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Directory path to list"
                        },
                        "recursive": {
                            "type": "boolean",
                            "description": "Recursively list subdirectories"
                        }
                    },
                    "required": ["path"]
                }),
                permissions: ToolPermissions {
                    read_files: true,
                    write_files: false,
                    execute_commands: false,
                    network_access: false,
                    git_operations: false,
                    allowed_paths: Vec::new(),
                    forbidden_paths: Vec::new(),
                },
            },
        }
    }
}

impl Default for ListFilesTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Tool for ListFilesTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    async fn execute(&self, params: &Value, context: &ToolContext) -> Result<ToolResult> {
        let path = params["path"]
            .as_str()
            .ok_or_else(|| KodError::InvalidParameters {
                reason: "Missing 'path' parameter".to_string(),
            })?;
        let recursive = params["recursive"].as_bool().unwrap_or(false);

        let resolved = context.resolve_path(path);
        context.can_read(&resolved)?;

        let mut files = Vec::new();

        if recursive {
            for entry in walkdir::WalkDir::new(&resolved)
                .into_iter()
                .filter_map(|e| e.ok())
            {
                files.push(entry.path().to_string_lossy().to_string());
            }
        } else {
            for entry in std::fs::read_dir(&resolved).map_err(KodError::Io)? {
                let entry = entry.map_err(KodError::Io)?;
                files.push(entry.path().to_string_lossy().to_string());
            }
        }

        files.sort();

        Ok(ToolResult::Success(serde_json::json!({
            "path": resolved.to_string_lossy().to_string(),
            "files": files,
        })))
    }
}

/// Search files for a pattern
pub struct GrepTool {
    pub definition: ToolDefinition,
}

impl GrepTool {
    pub fn new() -> Self {
        Self {
            definition: ToolDefinition {
                id: ToolId::new(),
                name: "grep".to_string(),
                description: "Search for a pattern in files".to_string(),
                category: ToolCategory::FileSystem,
                parameters_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Directory to search"
                        },
                        "pattern": {
                            "type": "string",
                            "description": "Pattern to search for"
                        },
                        "recursive": {
                            "type": "boolean",
                            "description": "Search recursively"
                        }
                    },
                    "required": ["path", "pattern"]
                }),
                permissions: ToolPermissions {
                    read_files: true,
                    write_files: false,
                    execute_commands: false,
                    network_access: false,
                    git_operations: false,
                    allowed_paths: Vec::new(),
                    forbidden_paths: Vec::new(),
                },
            },
        }
    }
}

impl Default for GrepTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Tool for GrepTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    async fn execute(&self, params: &Value, context: &ToolContext) -> Result<ToolResult> {
        let path = params["path"]
            .as_str()
            .ok_or_else(|| KodError::InvalidParameters {
                reason: "Missing 'path' parameter".to_string(),
            })?;
        let pattern = params["pattern"]
            .as_str()
            .ok_or_else(|| KodError::InvalidParameters {
                reason: "Missing 'pattern' parameter".to_string(),
            })?;
        let recursive = params["recursive"].as_bool().unwrap_or(false);

        let resolved = context.resolve_path(path);
        context.can_read(&resolved)?;

        let glob = if recursive {
            format!("{}/**", path)
        } else {
            format!("{}/*", path)
        };

        let matcher = globset::GlobSetBuilder::new()
            .add(
                globset::Glob::new(&glob)
                    .map_err(|e| KodError::InvalidState(format!("Invalid glob: {}", e)))?,
            )
            .build()
            .map_err(|e| KodError::InvalidState(format!("Invalid globset: {}", e)))?;

        let mut results = Vec::new();

        for entry in walkdir::WalkDir::new(&resolved)
            .into_iter()
            .filter_entry(|_| recursive)
            .filter_map(|e| e.ok())
        {
            let file_path = entry.path();
            if !matcher.is_match(file_path) {
                continue;
            }

            if file_path.is_file() && std::fs::read_to_string(file_path).is_ok() {
                let content = std::fs::read_to_string(file_path).unwrap();
                for (line_num, line) in content.lines().enumerate() {
                    if line.contains(pattern) {
                        results.push(serde_json::json!({
                            "file": file_path.to_string_lossy().to_string(),
                            "line": line_num + 1,
                            "text": line.trim(),
                        }));
                    }
                }
            }
        }

        Ok(ToolResult::Success(serde_json::json!({
            "pattern": pattern,
            "results": results,
        })))
    }
}

/// Get file information
pub struct FileInfoTool {
    pub definition: ToolDefinition,
}

impl FileInfoTool {
    pub fn new() -> Self {
        Self {
            definition: ToolDefinition {
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
                    git_operations: false,
                    allowed_paths: Vec::new(),
                    forbidden_paths: Vec::new(),
                },
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

        let resolved = context.resolve_path(path);
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
