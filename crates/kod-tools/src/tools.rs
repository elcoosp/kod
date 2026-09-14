//! Built-in tools for common agent operations.

use crate::{Tool, ToolContext};
use kod_error::{KodError, Result};
use kod_types::{ToolCategory, ToolDefinition, ToolId, ToolPermissions, ToolResult};
use regex::Regex;
use serde_json::Value;

/// Byte cap for `read_file`. Files larger than this are truncated at a
/// UTF-8 boundary and the result carries `"truncated": true`. 256 KB is
/// enough for a mid-size source file; the engine's 4000-char prompt cap
/// still applies on top of this when the result is fed back to the model.
const MAX_READ_BYTES: usize = 256 * 1024;

/// Byte cap per stream for `execute_command`. When either stdout or
/// stderr exceeds this, the child is killed — a runaway command like
/// `yes` or `find /` would otherwise exhaust memory and wedge the pipe.
const MAX_CMD_OUTPUT_BYTES: usize = 64 * 1024;

/// Read up to `cap` bytes from an async reader. Returns the bytes read
/// and whether the source had more (reading hit the cap).
async fn read_capped<R>(reader: &mut R, cap: usize) -> std::io::Result<(Vec<u8>, bool)>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    let mut buf: Vec<u8> = Vec::with_capacity(cap.min(8192));
    let mut limited = reader.take(cap as u64 + 1);
    limited.read_to_end(&mut buf).await?;
    let truncated = buf.len() > cap;
    if truncated {
        buf.truncate(cap);
    }
    Ok((buf, truncated))
}

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

        // Read with a hard byte cap instead of `read_to_string`, so a
        // huge file (log, generated lock file, binary) can't exhaust
        // memory before the engine's prompt-side truncation kicks in.
        use std::io::Read as _;
        let mut file = std::fs::File::open(&resolved).map_err(KodError::Io)?;
        let mut buf: Vec<u8> = Vec::with_capacity(8192);
        file.by_ref()
            .take(MAX_READ_BYTES as u64 + 1)
            .read_to_end(&mut buf)
            .map_err(KodError::Io)?;
        let truncated = buf.len() > MAX_READ_BYTES;
        if truncated {
            buf.truncate(MAX_READ_BYTES);
        }
        // Truncation may have landed mid-UTF-8; drop the trailing
        // incomplete char instead of returning invalid bytes.
        let content = match std::str::from_utf8(&buf) {
            Ok(s) => s.to_string(),
            Err(e) => String::from_utf8_lossy(&buf[..e.valid_up_to()]).into_owned(),
        };

        Ok(ToolResult::Success(serde_json::json!({
            "path": resolved.to_string_lossy().to_string(),
            "content": content,
            "truncated": truncated,
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

        // Spawn with piped stdio so each stream is capped independently
        // and the child is killed the moment output runs away.
        let mut child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(KodError::Io)?;

        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| KodError::Internal("child stdout missing".to_string()))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| KodError::Internal("child stderr missing".to_string()))?;

        // Read both streams concurrently and kill the child the moment
        // EITHER exceeds its cap. A naive tokio::join! deadlocks here:
        // the child holds each pipe's write end open until it exits, so
        // when stdout hits its cap and returns, the stderr future is
        // still parked on read_to_end waiting for EOF from a child that
        // nothing is terminating. (The original `yes`-style test hung
        // for exactly this reason.) Killing on the first over-cap
        // result unblocks the other read.
        //
        // Bounded by context.timeout_secs so a command that produces no
        // output but never exits (sleep 9999) is still terminated.
        let stdout_fut = read_capped(&mut stdout, MAX_CMD_OUTPUT_BYTES);
        let stderr_fut = read_capped(&mut stderr, MAX_CMD_OUTPUT_BYTES);
        tokio::pin!(stdout_fut);
        tokio::pin!(stderr_fut);

        let mut stdout_res: Option<std::io::Result<(Vec<u8>, bool)>> = None;
        let mut stderr_res: Option<std::io::Result<(Vec<u8>, bool)>> = None;

        let timeout = tokio::time::sleep(std::time::Duration::from_secs(
            context.timeout_secs.max(1),
        ));
        tokio::pin!(timeout);
        let mut timed_out = false;

        loop {
            if stdout_res.is_some() && stderr_res.is_some() {
                break;
            }
            if timed_out {
                // Child was killed; drain the reads to EOF and exit.
                // The other branch below will still fire because the
                // child's death closes its pipe ends.
            }
            tokio::select! {
                r = &mut stdout_fut, if stdout_res.is_none() && !timed_out => {
                    let over_cap = matches!(&r, Ok((_, true)));
                    stdout_res = Some(r);
                    if over_cap && stderr_res.is_none() {
                        let _ = child.start_kill();
                    }
                }
                r = &mut stderr_fut, if stderr_res.is_none() && !timed_out => {
                    let over_cap = matches!(&r, Ok((_, true)));
                    stderr_res = Some(r);
                    if over_cap && stdout_res.is_none() {
                        let _ = child.start_kill();
                    }
                }
                _ = &mut timeout, if !timed_out => {
                    timed_out = true;
                    let _ = child.start_kill();
                }
            }
        }

        let (stdout_bytes, stdout_truncated) = stdout_res
            .expect("loop exits only when stdout_res is set")
            .map_err(KodError::Io)?;
        let (stderr_bytes, stderr_truncated) = stderr_res
            .expect("loop exits only when stderr_res is set")
            .map_err(KodError::Io)?;

        let status = child.wait().await.map_err(KodError::Io)?;

        Ok(ToolResult::Success(serde_json::json!({
            "stdout": String::from_utf8_lossy(&stdout_bytes).to_string(),
            "stderr": String::from_utf8_lossy(&stderr_bytes).to_string(),
            "exit_code": status.code().unwrap_or(-1),
            "stdout_truncated": stdout_truncated,
            "stderr_truncated": stderr_truncated,
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
                description: "Search file contents with a regular expression. Respects .gitignore (skips target/, node_modules/, .git, …); results cap at 500 matches. Use \\b, \\w, [abc], (a|b), etc. — not PCRE lookarounds.".to_string(),
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
                            "description": "Rust `regex` crate pattern. Metacharacters are active; escape them (e.g. \\.) to match literally."
                        },
                        "recursive": {
                            "type": "boolean",
                            "description": "Search recursively"
                        },
                        "case_insensitive": {
                            "type": "boolean",
                            "description": "Match case-insensitively (default false, matching grep). Set true when the case of the target is unknown."
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
        let case_insensitive = params["case_insensitive"].as_bool().unwrap_or(false);

        let resolved = context.resolve_path(path)?;
        context.can_read(&resolved)?;

        // Compile the caller's pattern as a regex. An invalid pattern
        // becomes a ToolResult::Error so the model sees its own mistake
        // instead of the whole tool loop stalling.
        let regex = match Regex::new(pattern) {
            Ok(r) => r,
            Err(e) => {
                return Ok(ToolResult::Error(format!(
                    "invalid regex {:?}: {}",
                    pattern, e
                )));
            }
        };
        // `regex` has no inline (?i) rebuild helper, so recompile with
        // the case-insensitive flag when requested.
        let regex = if case_insensitive {
            let folded = format!("(?i){}", pattern);
            match Regex::new(&folded) {
                Ok(r) => r,
                Err(e) => {
                    return Ok(ToolResult::Error(format!(
                        "invalid regex {:?}: {}",
                        pattern, e
                    )));
                }
            }
        } else {
            regex
        };

        // `gitaware_walk` already roots at `resolved` and depth-limits
        // when non-recursive, so its yielded paths are the search set.
        // The old code additionally filtered with a glob built from the
        // *user-supplied* `path` — which never matched the absolute
        // paths the walker returns, so grep silently returned nothing
        // for relative-path calls. Dropped.
        let mut results = Vec::new();

        for file_path in gitaware_walk(&resolved, recursive) {
            if results.len() >= MAX_GREP_MATCHES {
                break;
            }
            if !file_path.is_file() {
                continue;
            }
            let content = match std::fs::read_to_string(&file_path) {
                Ok(c) => c,
                Err(_) => continue,
            };
            for (line_num, line) in content.lines().enumerate() {
                if regex.is_match(line) {
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

        Ok(ToolResult::Success(serde_json::json!({
            "pattern": pattern,
            "case_insensitive": case_insensitive,
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

#[cfg(test)]
mod tests {
    use super::*;
    use kod_types::ToolPermissions;

    fn full_context(dir: &std::path::Path) -> ToolContext {
        ToolContext::new(dir).with_permissions(ToolPermissions {
            read_files: true,
            write_files: true,
            execute_commands: true,
            ..Default::default()
        })
    }

    #[tokio::test]
    async fn read_file_truncates_oversized_files() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("big.txt");
        let content = "x".repeat(MAX_READ_BYTES + 1024);
        std::fs::write(&path, &content).unwrap();

        let ctx = full_context(temp.path());
        let tool = ReadFileTool::new();
        let params = serde_json::json!({ "path": "big.txt" });
        let result = tool.execute(&params, &ctx).await.unwrap();

        match result {
            ToolResult::Success(v) => {
                assert_eq!(v["truncated"], true);
                let body = v["content"].as_str().unwrap();
                assert_eq!(body.len(), MAX_READ_BYTES);
            }
            other => panic!("expected success, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn read_file_small_file_is_not_truncated() {
        let temp = tempfile::TempDir::new().unwrap();
        std::fs::write(temp.path().join("small.txt"), "hello world").unwrap();

        let ctx = full_context(temp.path());
        let tool = ReadFileTool::new();
        let params = serde_json::json!({ "path": "small.txt" });
        let result = tool.execute(&params, &ctx).await.unwrap();

        match result {
            ToolResult::Success(v) => {
                assert_eq!(v["truncated"], false);
                assert_eq!(v["content"], "hello world");
            }
            other => panic!("expected success, got {:?}", other),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn execute_command_truncates_runaway_output() {
        let temp = tempfile::TempDir::new().unwrap();
        let ctx = full_context(temp.path());
        let tool = ExecuteCommandTool::new();
        // `yes` prints "y\n" forever; the cap should kick in fast and
        // the child should be killed rather than wedging the pipe.
        let params = serde_json::json!({ "command": "yes" });
        let result = tool.execute(&params, &ctx).await.unwrap();

        match result {
            ToolResult::Success(v) => {
                assert_eq!(v["stdout_truncated"], true);
                let out = v["stdout"].as_str().unwrap();
                assert!(out.len() <= MAX_CMD_OUTPUT_BYTES);
                // `take(cap + 1)` reads one extra byte then we truncate,
                // so the returned length is exactly the cap.
                assert_eq!(out.len(), MAX_CMD_OUTPUT_BYTES);
            }
            other => panic!("expected success, got {:?}", other),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn execute_command_small_output_is_not_truncated() {
        let temp = tempfile::TempDir::new().unwrap();
        let ctx = full_context(temp.path());
        let tool = ExecuteCommandTool::new();
        let params = serde_json::json!({ "command": "echo hello" });
        let result = tool.execute(&params, &ctx).await.unwrap();

        match result {
            ToolResult::Success(v) => {
                assert_eq!(v["stdout_truncated"], false);
                assert_eq!(v["stdout"], "hello\n");
                assert_eq!(v["exit_code"], 0);
            }
            other => panic!("expected success, got {:?}", other),
        }
    }

    fn grep_ctx(dir: &std::path::Path) -> ToolContext {
        ToolContext::new(dir).with_permissions(ToolPermissions {
            read_files: true,
            ..Default::default()
        })
    }

    #[tokio::test]
    async fn grep_finds_literal_pattern_from_relative_path() {
        // Regression: the previous implementation built a glob from the
        // user-supplied relative `path` and matched it against the
        // absolute paths returned by the walker, so calling grep with
        // path="." or path="src" silently returned zero matches.
        let temp = tempfile::TempDir::new().unwrap();
        std::fs::write(temp.path().join("a.txt"), "needle\nhay\n").unwrap();
        std::fs::write(temp.path().join("b.txt"), "hay only\n").unwrap();

        let ctx = grep_ctx(temp.path());
        let tool = GrepTool::new();
        let params = serde_json::json!({ "path": ".", "pattern": "needle" });
        let result = tool.execute(&params, &ctx).await.unwrap();

        match result {
            ToolResult::Success(v) => {
                let hits = v["results"].as_array().unwrap();
                assert_eq!(hits.len(), 1, "got {:?}", hits);
                assert_eq!(hits[0]["text"], "needle");
            }
            other => panic!("expected success, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn grep_treats_pattern_as_regex() {
        let temp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            temp.path().join("code.rs"),
            "fn main() {}\nfn helper(x: u32) -> u32 { x }\nlet n = 42;\n",
        )
        .unwrap();

        let ctx = grep_ctx(temp.path());
        let tool = GrepTool::new();
        // Match any `fn <name>(` definition.
        let params = serde_json::json!({
            "path": ".",
            "pattern": r"fn\s+\w+\s*\("
        });
        let result = tool.execute(&params, &ctx).await.unwrap();

        match result {
            ToolResult::Success(v) => {
                let hits = v["results"].as_array().unwrap();
                assert_eq!(hits.len(), 2, "got {:?}", hits);
            }
            other => panic!("expected success, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn grep_case_insensitive_flag_works() {
        let temp = tempfile::TempDir::new().unwrap();
        std::fs::write(temp.path().join("mixed.txt"), "TODO\ntodo\nTodo\n").unwrap();

        let ctx = grep_ctx(temp.path());
        let tool = GrepTool::new();

        let params = serde_json::json!({
            "path": ".",
            "pattern": "todo"
        });
        let result = tool.execute(&params, &ctx).await.unwrap();
        match result {
            ToolResult::Success(v) => {
                assert_eq!(v["results"].as_array().unwrap().len(), 1);
            }
            other => panic!("expected success, got {:?}", other),
        }

        let params = serde_json::json!({
            "path": ".",
            "pattern": "todo",
            "case_insensitive": true
        });
        let result = tool.execute(&params, &ctx).await.unwrap();
        match result {
            ToolResult::Success(v) => {
                assert_eq!(v["results"].as_array().unwrap().len(), 3);
            }
            other => panic!("expected success, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn grep_invalid_regex_returns_error_result() {
        let temp = tempfile::TempDir::new().unwrap();
        std::fs::write(temp.path().join("a.txt"), "anything").unwrap();

        let ctx = grep_ctx(temp.path());
        let tool = GrepTool::new();
        let params = serde_json::json!({ "path": ".", "pattern": "[" });
        let result = tool.execute(&params, &ctx).await.unwrap();

        match result {
            ToolResult::Error(msg) => {
                assert!(msg.contains("invalid regex"), "got: {msg}");
            }
            other => panic!("expected ToolResult::Error, got {:?}", other),
        }
    }
}
