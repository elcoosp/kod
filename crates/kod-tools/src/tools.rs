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

        let resolved = context.resolve_path(path)?;
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

        let resolved = context.resolve_path(path)?;
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
                description: "List files in a directory. Respects .gitignore (skips target/, node_modules/, .git, …); results cap at 5000 entries".to_string(),
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

        let resolved = context.resolve_path(path)?;
        context.can_read(&resolved)?;

        let mut files: Vec<String> = gitaware_walk(&resolved, recursive)
            .into_iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();

        files.sort();

        let total = files.len();
        let truncated = total > MAX_LIST_ENTRIES;
        if truncated {
            files.truncate(MAX_LIST_ENTRIES);
        }

        Ok(ToolResult::Success(serde_json::json!({
            "path": resolved.to_string_lossy().to_string(),
            "files": files,
            "total": total,
            "truncated": truncated,
        })))
    }
}

/// Cap for directory listings: a recursive `list_files` over a repo with a
/// `target/` dir used to return 40k+ entries and blow the model context.
/// Results past the cap are dropped and reported via `truncated`.
const MAX_LIST_ENTRIES: usize = 5000;
/// Cap for grep matches for the same reason.
const MAX_GREP_MATCHES: usize = 500;

/// Walk `root` honoring `.gitignore`/`.ignore`/`.git/info/exclude` (plus
/// global git excludes), keeping dotfiles visible but always pruning `.git`.
/// Used by `list_files` and `grep` so ignored build output (`target/`,
/// `node_modules/`, …) never bloats tool results.
fn gitaware_walk(root: &std::path::Path, recursive: bool) -> Vec<std::path::PathBuf> {
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .hidden(false)
        .git_ignore(true)
        .ignore(true)
        .git_global(true)
        .git_exclude(true)
        // Honor .gitignore files even outside a git checkout: the tool's
        // contract is filesystem-based, not repo-based.
        .require_git(false)
        .filter_entry(|e| e.file_name().to_str().is_some_and(|n| n != ".git"));
    if !recursive {
        builder.max_depth(Some(1));
    }
    builder
        .build()
        .filter_map(|e| e.ok())
        .map(|e| e.into_path())
        // The walker yields the root itself as its first entry — callers
        // want the root's children, not the root.
        .filter(|p| p != root)
        .collect()
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

        let resolved = context.resolve_path(path)?;
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

        for file_path in gitaware_walk(&resolved, recursive) {
            if !matcher.is_match(&file_path) {
                continue;
            }

            if results.len() >= MAX_GREP_MATCHES {
                break;
            }
            if file_path.is_file() {
                let content = match std::fs::read_to_string(&file_path) {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                for (line_num, line) in content.lines().enumerate() {
                    if line.contains(pattern) {
                        results.push(serde_json::json!({
                            "file": file_path.to_string_lossy().to_string(),
                            "line": line_num + 1,
                            "text": line.trim(),
                        }));
                        if results.len() >= MAX_GREP_MATCHES {
                            break;
                        }
                    }
                }
            }
        }

        Ok(ToolResult::Success(serde_json::json!({
            "pattern": pattern,
            "results": results,
            "truncated": results.len() >= MAX_GREP_MATCHES,
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
