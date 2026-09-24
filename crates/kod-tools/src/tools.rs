//! Built-in tools for common agent operations.

use crate::{Tool, ToolContext};
// P8: the SearchBackend trait must be in scope for the
// ripgrep fast path to call `.search()` on the backend.
#[allow(unused_imports)]
use crate::relevance::SearchBackend;
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

/// Map a path-level IO error into a message the model can act on.
///
/// The default Display of std::io::Error on a directory read is
/// "Is a directory (os error 21)" — technically accurate, but it does
/// not tell the model what to do next. Same for the bare "No such file
/// or directory" and "Permission denied". Return a short label plus a
/// one-line suggestion, so the model recovers instead of giving up or
/// (worse) re-issuing the same call.
fn describe_path_error(path: &std::path::Path, err: &std::io::Error) -> String {
    let kind = err.kind();
    let p = path.display();
    match kind {
        std::io::ErrorKind::NotFound => format!(
            "not found: {p}. Check the path — a typo or a directory you have \
             not listed yet is the common cause.",
        ),
        std::io::ErrorKind::PermissionDenied => {
            format!("permission denied: {p}. The process does not have read access.",)
        }
        std::io::ErrorKind::IsADirectory => format!(
            "is a directory, not a file: {p}. Use list_files to see its \
             contents, or read a specific file inside it.",
        ),
        _ => {
            // Windows reports directory reads as "other"; check metadata
            // to give the same advice.
            if path.is_dir() {
                format!(
                    "is a directory, not a file: {p}. Use list_files to see \
                     its contents, or read a specific file inside it.",
                )
            } else {
                format!("{}: {p}", err)
            }
        }
    }
}

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
/// Heuristic: does this environment variable name look like it holds
/// a credential? Used by `execute_command` to strip secrets from the
/// child's environment. The matcher is deliberately broad — the cost
/// of a false positive is that a subprocess does not see a variable
/// it did not need; the cost of a false negative is credential leak.
///
/// Covered: anything with `KEY`, `TOKEN`, `SECRET`, `PASSWORD`, `PASSWD`,
/// `CREDENTIAL`, `AUTH`, `BEARER` in the name (case-insensitive), plus
/// the well-known `*_API_KEY` / `ANTHROPIC_*` / `OPENAI_*` / `AWS_*` /
/// `GCP_*` / `GOOGLE_*` / `AZURE_*` prefixes. `PATH`, `HOME`, `LANG`,
/// `TMPDIR`, `PWD`, `SHELL`, `TERM`, and the `LC_*` family are
/// explicitly allowed.
fn is_secret_like_env(name: &str) -> bool {
    // Allow-list for the shell's own furniture, checked first so an
    // override below never strips them by accident.
    const ALLOW: &[&str] = &[
        "PATH", "HOME", "USER", "LOGNAME", "SHELL", "TERM", "PWD", "OLDPWD", "LANG", "TMPDIR",
        "TMP", "TEMP",
    ];
    if ALLOW.contains(&name) {
        return false;
    }
    if name.starts_with("LC_") {
        return false;
    }

    const TOKENS: &[&str] = &[
        "KEY",
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "PASSWD",
        "CREDENTIAL",
        "AUTH",
        "BEARER",
        "PRIVATE",
    ];
    let upper = name.to_ascii_uppercase();
    if TOKENS.iter().any(|t| upper.contains(t)) {
        return true;
    }
    const PREFIXES: &[&str] = &["ANTHROPIC_", "OPENAI_", "AWS_", "GCP_", "GOOGLE_", "AZURE_"];
    PREFIXES.iter().any(|p| upper.starts_with(p))
}

/// H-R13: atomic file write. `std::fs::write` truncates in place; a
/// crash or ENOSPC mid-write leaves a torn file that the engine then
/// feeds back to the model. A same-directory temp file + fsync + rename
/// is atomic on POSIX (and on Windows, `rename` over an existing file
/// is atomic since Rust 1.55).
///
/// The temp file name is `.kod-tmp-<pid>-<nanos>` in the destination
/// directory (same filesystem — a cross-fs rename is a copy + delete,
/// not atomic). On any error the temp file is removed.
fn atomic_write(path: &std::path::Path, content: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = parent.join(format!(".kod-tmp-{}-{:x}", std::process::id(), nanos));
    let result = (|| -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(content)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

pub struct ReadFileTool {
    pub definition: ToolDefinition,
}

impl ReadFileTool {
    pub fn new() -> Self {
        Self {
            definition: ToolDefinition {
                trust_level: kod_types::trust::TrustLevel::default(),
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
                    git_access: kod_types::GitAccess::None,
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

        // Delta §7.5: internal-URL dispatch. When the path carries a
        // scheme the router knows (`artifact://`, `memory://`, …) the
        // read is a handler call, not a filesystem access. The check
        // happens before `resolve_path` because that method would
        // interpret the URL as a relative filesystem path and mangle
        // it into `<cwd>/artifact://foo`.
        //
        // The tool's own `read_files` permission still gates the
        // whole call — the router is a dispatch layer, not a
        // capability — so a session that cannot read files also
        // cannot read artifacts.
        if let Some(router) = context.protocol_router.as_ref()
            && router.handles(path)
        {
            let rctx = crate::internal_url::ResolveContext::new(
                context.holder.clone(),
                context.working_dir.clone(),
            );
            return match router.resolve(path, &rctx).await {
                Ok(r) => Ok(ToolResult::Success(serde_json::json!({
                    "path": path,
                    "content": r.text,
                    "mime": r.mime,
                    "immutable": r.immutable,
                    "source": "internal-url",
                }))),
                Err(e) => Ok(ToolResult::Error(e.to_string())),
            };
        }

        let resolved = context.resolve_path(path)?;
        // Tier 1.3 — known-secret path check.
        if let Some(rp) = context.read_protection.as_ref()
            && rp.matches(&resolved)
        {
            use kod_config::ReadMode;
            if let ReadMode::Refuse = rp.mode {
                return Ok(ToolResult::Error(format!(
                    "read_file refused: {} matches a read-protection pattern. \
                     Edit [policy.read_protection] in the config to allow.",
                    resolved.display(),
                )));
            }
        }
        context.can_read(&resolved)?;
        // P1-c: a read is a read, whether the file turns out to be
        // text, binary, or truncated. Firing here (after the
        // permission check, before the content probe) means every
        // success path — including the redact path — records the
        // touch exactly once.
        context.note_file_touch(
            &resolved,
            crate::context::FileOp::Read,
            params["intent"].as_str(),
        );

        // Total size from metadata (the byte cap below can hide it).
        let total_size = std::fs::metadata(&resolved).map(|m| m.len()).unwrap_or(0);

        // Reject directories up front with a structured message. The
        // downstream open would fail with "Is a directory" (or a
        // Windows-specific "other" error), which the model cannot
        // distinguish from a missing file or a permission problem. A
        // clear "is a directory — use list_files" is the recovery the
        // model actually needs.
        if resolved.is_dir() {
            return Ok(ToolResult::Error(describe_path_error(
                &resolved,
                &std::io::Error::new(std::io::ErrorKind::IsADirectory, "is a directory"),
            )));
        }

        // Read with a hard byte cap instead of `read_to_string`, so a
        // huge file (log, generated lock file, binary) can't exhaust
        // memory before the engine's prompt-side truncation kicks in.
        use std::io::Read as _;
        // Missing files are a model mistake, not a tool-level I/O
        // failure. Return them as Ok(ToolResult::Error(...)) so the
        // message reaches the model as the tool's answer, matching
        // the directory case above. The previous code propagated
        // KodError::Io, which the engine turns into `error: <msg>` in
        // the tool result block — same content, but a different shape
        // in the type system, and inconsistent with how directories
        // were handled. One shape for "the model should reason about
        // this," one shape for "the tool itself failed."
        let mut file = match std::fs::File::open(&resolved) {
            Ok(f) => f,
            Err(e) => {
                return Ok(ToolResult::Error(describe_path_error(&resolved, &e)));
            }
        };
        let mut buf: Vec<u8> = Vec::with_capacity(8192);
        file.by_ref()
            .take(MAX_READ_BYTES as u64 + 1)
            .read_to_end(&mut buf)
            .map_err(KodError::Io)?;
        let truncated = buf.len() > MAX_READ_BYTES;
        if truncated {
            buf.truncate(MAX_READ_BYTES);
        }

        // Detect binary content before treating the bytes as text. A
        // NUL byte in the first 1 KB is the standard heuristic for
        // "not text" (git, file(1), ripgrep). The previous code fed
        // the bytes through `from_utf8_lossy`, so a PNG or compiled
        // object came back to the model as a wall of U+FFFD
        // replacement characters, or a short prefix ending at the
        // first invalid sequence — with no indication that the content
        // was anything other than a text file the user had asked about.
        //
        // A UTF-16 file also has NUL bytes (each ASCII char is
        // `XX 00`), and would land here. That is not a regression:
        // the previous behavior decoded UTF-16 as mojibake too,
        // because the crate treats everything as UTF-8. Returning the
        // honest "binary, here is a hex preview" is at least useful
        // for identifying what the file is; if UTF-16 support is
        // wanted, it belongs in a `file(1)`-style encoding probe that
        // this tool does not yet have.
        let probe_len = buf.len().min(1024);
        let looks_binary = buf[..probe_len].contains(&0u8);
        if looks_binary {
            // 64 bytes is enough for a magic number (`\x89PNG\r\n\x1a\n`,
            // `\x7fELF`, `PK\x03\x04`) and any short ASCII banner the
            // model might use to identify the format.
            let preview_len = buf.len().min(64);
            let preview_hex: String = buf[..preview_len]
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<Vec<_>>()
                .join(" ");
            return Ok(ToolResult::Success(serde_json::json!({
                "path": resolved.to_string_lossy().to_string(),
                "binary": true,
                "size_bytes": total_size,
                "truncated": truncated,
                "preview_hex": preview_hex,
            })));
        }

        // Truncation may have landed mid-UTF-8; drop the trailing
        // incomplete char instead of returning invalid bytes.
        let content = match std::str::from_utf8(&buf) {
            Ok(s) => s.to_string(),
            Err(e) => String::from_utf8_lossy(&buf[..e.valid_up_to()]).into_owned(),
        };

        // Tier 1.3 — sanitize the content when the path matched a
        // read-protection pattern and the mode is `Redact`.
        let content = redact_read_content(&content, context, &resolved);

        Ok(ToolResult::Success(serde_json::json!({
            "path": resolved.to_string_lossy().to_string(),
            "content": content,
            "truncated": truncated,
            "binary": false,
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
                trust_level: kod_types::trust::TrustLevel::default(),
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
                    git_access: kod_types::GitAccess::None,
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

        // Delta §7.5: internal-URL dispatch for schemes that support
        // a write. A handler that is read-only returns `ReadOnly`
        // and this branch produces a clear refusal; the model is
        // told the URL cannot be written rather than the write
        // silently doing nothing.
        //
        // The `append` parameter is currently ignored for internal
        // URLs: the one write-capable handler this commit ships
        // (`conflict://`, in a later slice) is a full-replacement
        // API. A future handler that cares can read it from `params`
        // in its own `write` impl, which is where the semantics
        // belong.
        if let Some(router) = context.protocol_router.as_ref()
            && router.handles(path)
        {
            let content = params["content"]
                .as_str()
                .ok_or_else(|| KodError::InvalidParameters {
                    reason: "Missing 'content' parameter".to_string(),
                })?;
            let rctx = crate::internal_url::ResolveContext::new(
                context.holder.clone(),
                context.working_dir.clone(),
            );
            return match router.write(path, content, &rctx).await {
                Ok(()) => Ok(ToolResult::Success(serde_json::json!({
                    "path": path,
                    "bytes": content.len(),
                    "source": "internal-url",
                }))),
                Err(e) => Ok(ToolResult::Error(e.to_string())),
            };
        }

        let content = params["content"]
            .as_str()
            .ok_or_else(|| KodError::InvalidParameters {
                reason: "Missing 'content' parameter".to_string(),
            })?;
        let append = params["append"].as_bool().unwrap_or(false);

        let resolved = context.resolve_path(path)?;
        context.can_write(&resolved)?;

        // Advisory per-path lock, when the context carries a table.
        // Two writers on the same canonical path serialize here: the
        // second waits up to `context.lock_timeout`, then fails with a
        // message the model can act on. Without a table (a bare
        // context in a unit test, or a caller that never installed
        // one), the write proceeds unguarded — the behavior every tool
        // had before this field existed.
        //
        // The guard is dropped at the end of the function, after the
        // write; holding it across the whole body is intentional, so
        // the parent-directory creation and the file write happen
        // under the same lock.
        let _lock = match &context.lock_table {
            Some(table) => match table
                .acquire(&resolved, &context.holder, context.lock_timeout)
                .await
            {
                Ok(guard) => Some(guard),
                Err(e) => {
                    return Ok(ToolResult::Error(format!(
                        "cannot write {}: {}",
                        resolved.display(),
                        e
                    )));
                }
            },
            None => None,
        };

        // Create parent directories if the target is in a new tree.
        // The model frequently writes to a fresh path — a new module
        // under `src/handlers/`, a first skill file under
        // `~/.agents/skills/` — and `std::fs::write` fails with ENOENT
        // when the parent does not exist. The model's natural response
        // is to give up, because it has no tool for creating
        // directories. Idempotent and cheap: `create_dir_all` on an
        // existing tree is a no-op.
        //
        // Failures come back as Ok(ToolResult::Error(...)) rather than
        // Err(KodError::Io): every one of them is a "the write could
        // not be done, here is why, here is what to check" — the shape
        // the model should reason about — rather than "the tool itself
        // broke." Same shape the read_file fix uses, via the same
        // describe_path_error helper. The engine treats both shapes
        // the same when it renders the tool result block, so the
        // observable difference is only in the type system; the
        // consistent shape is what lets a future caller distinguish
        // model-facing errors from tool-level failures without
        // inspecting the message text.
        if let Some(parent) = resolved.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            return Ok(ToolResult::Error(describe_path_error(parent, &e)));
        }

        if append {
            let mut file = match std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&resolved)
            {
                Ok(f) => f,
                Err(e) => {
                    return Ok(ToolResult::Error(describe_path_error(&resolved, &e)));
                }
            };
            use std::io::Write;
            if let Err(e) = write!(file, "{}", content) {
                return Ok(ToolResult::Error(describe_path_error(&resolved, &e)));
            }
        } else if let Err(e) = atomic_write(&resolved, content.as_bytes()) {
            return Ok(ToolResult::Error(describe_path_error(&resolved, &e)));
        }

        // P1-c: fire only after the write is durable. A failed write
        // returns above and produces no touch — the file is unchanged,
        // so there is nothing for a peer to know about.
        context.note_file_touch(
            &resolved,
            crate::context::FileOp::Write,
            params["intent"].as_str(),
        );

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
                trust_level: kod_types::trust::TrustLevel::default(),
                id: ToolId::new(),
                name: "execute_command".to_string(),
                description: "Execute a shell command via `sh -c` on Unix and `cmd /C` on Windows. The command runs in the working directory and inherits no shell aliases or profile; write POSIX syntax on Unix and cmd.exe syntax on Windows.".to_string(),
                category: ToolCategory::System,
                parameters_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "command": {
                            "type": "string",
                            "description": "Command to execute"
                        },
                        "run_in_background": {
                            "type": "boolean",
                            "description": "Start the command and return immediately. \
                                            Output goes to a spool file; the job \
                                            reports completion and stalls through \
                                            a background interrupt."
                        },
                        "stall_wake_seconds": {
                            "type": "integer",
                            "description": "With run_in_background: notify if the \
                                            command produces no output for this many \
                                            seconds (minimum 30). Omit to disable."
                        }
                    },
                    "required": ["command"]
                }),
                permissions: ToolPermissions {
                    read_files: false,
                    write_files: false,
                    execute_commands: true,
                    network_access: false,
                    git_access: kod_types::GitAccess::None,
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

        // P2-d: a background command hands off to the engine's spawner
        // and returns a job id immediately. The engine owns the spool,
        // the job registry, and the soft-interrupt channel that reports
        // completion — the tool crate cannot see any of them, which is
        // why the hand-off is a hook.
        //
        // A context with no hook (a unit test, an embedder) runs the
        // command inline, so the flag degrades rather than failing.
        if params
            .get("run_in_background")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            let stall = params
                .get("stall_wake_seconds")
                .and_then(|v| v.as_u64());
            match &context.on_background_command {
                Some(hook) => match hook.spawn(command, stall, &context.holder) {
                    Some(job_id) => {
                        return Ok(ToolResult::Success(serde_json::json!({
                            "background": true,
                            "job_id": job_id,
                            "note": "started in the background; output goes to a \
                                     spool file. Completion and stalls arrive as \
                                     background notices.",
                        })));
                    }
                    None => {
                        // The engine's runner refused (shutting down,
                        // or a cap). Falling through to inline is the
                        // safe reading: the command the model asked
                        // for still runs, it just blocks.
                        tracing::warn!(
                            "background spawn declined; running inline",
                        );
                    }
                },
                None => {
                    tracing::debug!(
                        "run_in_background requested but no spawner is installed; \
                         running inline",
                    );
                }
            }
        }

        // Pick the platform shell. The previous code hard-coded `sh -c`,
        // which silently broke the x86_64-pc-windows-msvc release target
        // CI builds: spawn succeeded, `sh` was not found, and the caller
        // saw a generic "no such file or directory" with no hint that
        // the tool had chosen the wrong interpreter.
        //
        // `cmd /C` is the closest Windows analogue of `sh -c`: it runs
        // the command and exits. Neither shell loads a user profile, so
        // aliases and rc files are not in scope.
        let (shell, shell_flag) = if cfg!(windows) {
            ("cmd", "/C")
        } else {
            ("sh", "-c")
        };

        // Wrap the shell invocation in the platform primitive when the
        // context's sandbox mode says so. `Auto` (the default) uses the
        // best available primitive or silently falls through to no
        // sandbox; `Require` fails loudly when nothing is available.
        // See `SandboxResolver`.
        let resolver = crate::context::default_resolver();
        let sandbox_inv = match resolver.invocation(
            context.sandbox,
            &context.working_dir,
            crate::context::SandboxOpts::default(),
        ) {
            Ok(v) => v,
            Err(e) => {
                return Ok(ToolResult::Error(format!("sandbox invocation failed: {e}")));
            }
        };

        // Spawn with piped stdio so each stream is capped independently
        // and the child is killed the moment output runs away.
        //
        // Sandboxed path: the sandbox invocation ends with `--`
        // (separator between the sandbox's own args and the command to
        // run inside). The command to run is the platform shell
        // (`sh`/`cmd`) followed by its flag and the user's command —
        // so the shell program must be appended before the flag.
        //
        // Unsandboxed path: the shell program IS the program the
        // `Command` was built with, so only the flag and the command
        // follow.
        let mut spawn = match &sandbox_inv {
            Some(inv) => {
                let mut c = tokio::process::Command::new(&inv.program);
                c.args(&inv.args);
                // The `--` was already pushed by the sandbox builder;
                // the shell program comes next.
                c.arg(shell);
                c
            }
            None => tokio::process::Command::new(shell),
        };
        // H-S6: run in the context's working directory. The previous
        // shape never set `current_dir`, so the child inherited the
        // process cwd (typically $HOME for a daemon) and the model's
        // `cargo build` silently built the wrong tree. Only bwrap
        // happened to pass `--chdir`; Seatbelt and Disabled did not.
        //
        // H-S5: strip secret-shaped environment variables. The child
        // otherwise sees ANTHROPIC_API_KEY, OPENAI_API_KEY, Jev keys,
        // daemon tokens — a prompt-injected model can read them via
        // `env` or /proc/self/environ. The policy is "inherit a small
        // allowlist plus everything that is not obviously a secret";
        // that keeps `PATH`, `HOME`, `LANG`, and proxy vars working
        // while dropping the credential-shaped ones.
        spawn
            .arg(shell_flag)
            .arg(command)
            .current_dir(&context.working_dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        for (k, v) in std::env::vars_os() {
            if let Some(name) = k.to_str()
                && is_secret_like_env(name)
            {
                continue;
            }
            spawn.env(&k, &v);
        }
        // Re-apply stdio after the env loop (the env calls do not touch
        // it, but keeping the ordering explicit makes the intent clear).
        spawn
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        let mut child = spawn.spawn().map_err(KodError::Io)?;

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

        let effective_timeout_secs = context.timeout_secs.max(1);
        let timeout = tokio::time::sleep(std::time::Duration::from_secs(effective_timeout_secs));
        tokio::pin!(timeout);
        let mut timed_out = false;

        loop {
            if stdout_res.is_some() && stderr_res.is_some() {
                break;
            }
            // The two read arms are gated only on "this read has not
            // finished yet" — NOT on `!timed_out`. The previous code
            // included `!timed_out` in every read guard, so the
            // moment the timeout arm fired (set `timed_out = true`,
            // killed the child), the next loop iteration reached
            // `tokio::select!` with all three arms disabled and no
            // `else` — which panics with "all branches are disabled
            // and there is no else branch".
            //
            // The panic fired on the exact case the timeout exists
            // for: a command that produces no output and never exits
            // (e.g. `sleep 9999`). The runaway-output test used `yes`
            // and never hit it, because a read arm always completed
            // before the timeout had a chance to fire.
            //
            // With the reads polled after a timeout: the child is
            // dead, its death closes the pipe write ends, and each
            // read returns whatever bytes were buffered followed by
            // EOF. The timeout arm keeps `!timed_out` so the sleep
            // fires exactly once.
            tokio::select! {
                r = &mut stdout_fut, if stdout_res.is_none() => {
                    let over_cap = matches!(&r, Ok((_, true)));
                    stdout_res = Some(r);
                    if over_cap && stderr_res.is_none() {
                        let _ = child.start_kill();
                    }
                }
                r = &mut stderr_fut, if stderr_res.is_none() => {
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

        // On Unix, distinguish "exited with code N" from "killed by
        // signal N". A process terminated by the truncation path's
        // start_kill() reports a signal, not an exit code; a process
        // that finished on its own — even with a non-zero exit, like
        // `grep` returning 1 for no matches — reports a code. The
        // downstream summariser used to infer "killed" from
        // `exit_code != 0`, which mislabelled every `grep` result
        // whose output happened to be truncated. Reporting the signal
        // directly removes the guess. Windows does not have exit
        // signals in the same sense; the field is omitted there.
        #[cfg(unix)]
        let exit_signal: Option<i32> = {
            use std::os::unix::process::ExitStatusExt;
            status.signal()
        };
        #[cfg(not(unix))]
        let exit_signal: Option<i32> = None;

        Ok(ToolResult::Success(serde_json::json!({
            "stdout": String::from_utf8_lossy(&stdout_bytes).to_string(),
            "stderr": String::from_utf8_lossy(&stderr_bytes).to_string(),
            "exit_code": status.code().unwrap_or(-1),
            "exit_signal": exit_signal,
            "stdout_truncated": stdout_truncated,
            "stderr_truncated": stderr_truncated,
            // Explicit, not inferred. The timeout kill and the
            // output-cap kill both surface as a signal, and the
            // summariser needs to distinguish them: an `exit_signal`
            // alone says "killed", but not why. A timeout is user
            // action-required (raise the timeout, or run in the
            // background); a cap kill means the command was too
            // chatty and the partial output is still representative.
            "timed_out": timed_out,
            "timeout_secs": effective_timeout_secs,
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
                trust_level: kod_types::trust::TrustLevel::default(),
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
                    git_access: kod_types::GitAccess::None,
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

        // Distinguish file / directory / missing. The previous
        // implementation called `gitaware_walk` unconditionally, so a
        // file target produced `{"files": [], "total": 0}` — the exact
        // shape an empty directory produces. The model could not tell
        // "you gave me a file" (a caller mistake worth naming) from
        // "this directory is empty" (a legitimate result).
        //
        // All three cases return Success so the model sees a structured
        // answer rather than a forced error path. The `path_kind` field
        // names the state; the file case returns a single-entry list
        // because "what files are at this path?" has an obvious answer
        // for a file.
        if !resolved.exists() {
            return Ok(ToolResult::Error(describe_path_error(
                &resolved,
                &std::io::Error::new(std::io::ErrorKind::NotFound, "not found"),
            )));
        }
        if resolved.is_file() {
            let entry = truncate_entry(&resolved.to_string_lossy(), MAX_ENTRY_BYTES);
            return Ok(ToolResult::Success(serde_json::json!({
                "path": resolved.to_string_lossy().to_string(),
                "path_kind": "file",
                "files": [entry],
                "total": 1,
                "truncated": false,
            })));
        }

        // Directory branch: the pre-existing behavior.
        let mut files: Vec<String> = gitaware_walk(&resolved, recursive)
            .into_iter()
            .map(|p| truncate_entry(&p.to_string_lossy(), MAX_ENTRY_BYTES))
            .collect();

        files.sort();

        let total = files.len();
        let truncated = total > MAX_LIST_ENTRIES;
        if truncated {
            files.truncate(MAX_LIST_ENTRIES);
        }

        Ok(ToolResult::Success(serde_json::json!({
            "path": resolved.to_string_lossy().to_string(),
            "path_kind": "directory",
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

/// Per-entry length cap, in bytes, for both `list_files` path entries and
/// `grep` result lines. The workspace has generated paths and generated
/// line content in the wild (a bundler output file with 8 KB of inline
/// JSON on one line). A single entry that long dominates the tool's own
/// count cap and forces the engine's downstream prompt cap to discard
/// every later entry — the model then sees one long path and nothing
/// else. Truncating each entry keeps the count and lets the downstream
/// cap do its normal work.
///
/// 1 KB is generous for a path (typical: 40–120 bytes) and for a line of
/// code (typical: 20–200 bytes) while bounded enough that 5000 entries
/// cannot exceed ~5 MB even in the pathological case.
const MAX_ENTRY_BYTES: usize = 1024;

/// Per-file byte cap for `grep`. Files larger than this are skipped and
/// reported in the result's `skipped_large_files` list.
///
/// The pre-streaming implementation called `std::fs::read_to_string` on
/// every candidate file, so a 2 GB log — the exact file a user might
/// want to grep — would allocate the whole thing into memory and OOM
/// the process before the entry cap could fire. A source tree rarely
/// has a file over a megabyte, and a file that large rarely contains
/// the line-level pattern a coding agent is looking for; 8 MB is
/// generous for the useful case and cheap to bound the useless one.
const MAX_GREP_FILE_BYTES: u64 = 8 * 1024 * 1024;

/// Cap on the `skipped_large_files` list returned with a grep result.
/// A repo with a build tree full of large artifacts (a `target/` that
/// .gitignore does not cover, a vendored dataset, a `node_modules/`
/// with binary blobs) can have hundreds of files over
/// [`MAX_GREP_FILE_BYTES`]. Listing every one of them would reproduce
/// the exact problem the size cap was meant to solve — a tool result
/// dominated by paths. 50 is enough to answer "which files were too
/// big?" for the common case; `skipped_large_files_total` carries the
/// real count when more were skipped.
const MAX_SKIPPED_LARGE_FILES: usize = 50;

/// Truncate a UTF-8 string to at most `max` bytes at a char boundary,
/// appending an ellipsis when the string was cut. Local to this module
/// to avoid a cross-crate dependency on the engine's helper.
fn truncate_entry(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// Walk `root` honoring `.gitignore`/`.ignore`/`.git/info/exclude` (plus
/// global git excludes), keeping dotfiles visible but always pruning `.git`.
/// Used by `list_files` and `grep` so ignored build output (`target/`,
/// `node_modules/`, …) never bloats tool results.
pub(crate) fn gitaware_walk(root: &std::path::Path, recursive: bool) -> Vec<std::path::PathBuf> {
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

/// Apply a unified diff to an existing file.
///
/// The complement to `write_file`: a 2000-line file needs 2000 lines of
/// prompt to rewrite, a 10-line patch needs 30. On local models with an
/// 8k context window that is the difference between "can edit this file"
/// and "cannot". The patch is also a reviewable artifact, so a caller
/// can show every pending diff before anything touches disk.
pub struct PatchFileTool {
    pub definition: ToolDefinition,
}

impl PatchFileTool {
    pub fn new() -> Self {
        Self {
            definition: ToolDefinition {
                trust_level: kod_types::trust::TrustLevel::default(),
                id: ToolId::new(),
                name: "patch_file".to_string(),
                description: "Apply a unified diff to an existing file. The diff format is `--- a/path`, `+++ b/path`, then one or more `@@ -l,n +l,n @@` hunks with ` ` prefix for context, `-` for removal, `+` for addition. The patch must apply cleanly — a context mismatch is returned as an error with the line and text that failed to match.".to_string(),
                category: ToolCategory::FileSystem,
                parameters_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path to the file to patch"
                        },
                        "patch": {
                            "type": "string",
                            "description": "Unified diff to apply"
                        },
                        "dry_run": {
                            "type": "boolean",
                            "description": "When true, validate the patch and report the result without writing. Defaults to false."
                        }
                    },
                    "required": ["path", "patch"]
                }),
                permissions: ToolPermissions {
                    read_files: true,
                    write_files: true,
                    execute_commands: false,
                    network_access: false,
                    git_access: kod_types::GitAccess::None,
                    allowed_paths: Vec::new(),
                    forbidden_paths: Vec::new(),
                },
            },
        }
    }
}

impl Default for PatchFileTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Tool for PatchFileTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    async fn execute(&self, params: &Value, context: &ToolContext) -> Result<ToolResult> {
        let path = params["path"]
            .as_str()
            .ok_or_else(|| KodError::InvalidParameters {
                reason: "Missing 'path' parameter".to_string(),
            })?;
        let patch = params["patch"]
            .as_str()
            .ok_or_else(|| KodError::InvalidParameters {
                reason: "Missing 'patch' parameter".to_string(),
            })?;
        let dry_run = params["dry_run"].as_bool().unwrap_or(false);

        let resolved = context.resolve_path(path)?;
        context.can_read(&resolved)?;
        context.can_write(&resolved)?;

        // H-R11: acquire the path lock *first*, before reading the
        // original. The pre-fix order read + diffed unlocked and only
        // locked for the write, so two concurrent patch_file calls
        // both diffed against the same original; the second write
        // silently reverted the first. `write_file` by contrast
        // holds the lock across its whole body; the lock table
        // exists precisely to make this the rule.
        //
        // Dry runs still take the lock: a dry-run reads the original
        // and the caller wants a consistent answer even while a real
        // patch is in flight.
        let _lock = match &context.lock_table {
            Some(table) => match table
                .acquire(&resolved, &context.holder, context.lock_timeout)
                .await
            {
                Ok(guard) => Some(guard),
                Err(e) => {
                    return Ok(ToolResult::Error(format!(
                        "cannot patch {}: {}",
                        resolved.display(),
                        e
                    )));
                }
            },
            None => None,
        };

        let original = match std::fs::read_to_string(&resolved) {
            Ok(s) => s,
            Err(e) => {
                return Ok(ToolResult::Error(describe_path_error(&resolved, &e)));
            }
        };

        let patched = match crate::patch::apply_unified_diff(&original, patch) {
            Ok(s) => s,
            Err(e) => {
                return Ok(ToolResult::Error(format!("patch did not apply: {e}")));
            }
        };

        if dry_run {
            return Ok(ToolResult::Success(serde_json::json!({
                "path": resolved.to_string_lossy().to_string(),
                "dry_run": true,
                "applied": true,
                "old_size": original.len(),
                "new_size": patched.len(),
            })));
        }

        if let Err(e) = atomic_write(&resolved, patched.as_bytes()) {
            return Ok(ToolResult::Error(describe_path_error(&resolved, &e)));
        }

        // P1-c: a real patch is a modification; a dry run returned
        // above and does not touch the file, so it produces no event.
        context.note_file_touch(
            &resolved,
            crate::context::FileOp::Edit,
            params["intent"].as_str(),
        );

        Ok(ToolResult::Success(serde_json::json!({
            "path": resolved.to_string_lossy().to_string(),
            "dry_run": false,
            "applied": true,
            "old_size": original.len(),
            "new_size": patched.len(),
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
                trust_level: kod_types::trust::TrustLevel::default(),
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
                    git_access: kod_types::GitAccess::None,
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
        // Files whose size exceeded MAX_GREP_FILE_BYTES. Reported in
        // the result so the model knows the search was not exhaustive
        // and can decide whether to grep them specifically. Capped at
        // MAX_SKIPPED_LARGE_FILES with the true count in
        // `skipped_large_files_total` — an uncapped list would turn
        // into the very "tool result dominated by paths" problem the
        // size limit exists to prevent.
        let mut skipped_large_files: Vec<String> = Vec::new();
        let mut skipped_large_total: usize = 0;

        use std::io::BufRead as _;

        for file_path in gitaware_walk(&resolved, recursive) {
            if results.len() >= MAX_GREP_MATCHES {
                break;
            }
            if !file_path.is_file() {
                continue;
            }
            // Size check before opening. `read_to_string` used to load
            // the entire file into memory; a 2 GB log would OOM here.
            if let Ok(meta) = std::fs::metadata(&file_path)
                && meta.len() > MAX_GREP_FILE_BYTES
            {
                skipped_large_total += 1;
                if skipped_large_files.len() < MAX_SKIPPED_LARGE_FILES {
                    skipped_large_files.push(file_path.to_string_lossy().to_string());
                }
                continue;
            }
            let file = match std::fs::File::open(&file_path) {
                Ok(f) => f,
                Err(_) => continue,
            };
            let reader = std::io::BufReader::new(file);
            // `.lines()` yields Result<String>; a line containing
            // invalid UTF-8 (a binary file, a log with raw bytes)
            // produces Err and we stop scanning that file. The old
            // `read_to_string` failed the whole file on any bad byte;
            // the streaming form at least gets matches from the clean
            // prefix.
            for (line_num, line_result) in reader.lines().enumerate() {
                let line = match line_result {
                    Ok(l) => l,
                    Err(_) => break,
                };
                if regex.is_match(&line) {
                    // Cap the matched text per entry. A generated
                    // bundler output file with 8 KB of inline JSON
                    // on one line would otherwise produce a single
                    // 8 KB match and blow the model's prompt budget
                    // for the whole call. The `file` field is left
                    // untouched — paths are already short, and the
                    // model needs the real path to open the file.
                    results.push(serde_json::json!({
                        "file": file_path.to_string_lossy().to_string(),
                        "line": line_num + 1,
                        "text": truncate_entry(line.trim(), MAX_ENTRY_BYTES),
                    }));
                    if results.len() >= MAX_GREP_MATCHES {
                        break;
                    }
                }
            }
        }

        // P8: relevance-rank the matches before capping the output.
        // The loop already capped the raw count at MAX_GREP_MATCHES;
        // this pass keeps the matches a reader would want when the
        // cap forced a choice. Deterministic, term-overlap scoring
        // (no model call) from `relevance::rank_hits`.
        //
        // A search with no query terms in common with any line keeps
        // its original order — the scoring is a filter, not a
        // reordering of everything.
        if results.len() > 1 {
            let query_terms = pattern.split_whitespace().collect::<Vec<_>>();
            if !query_terms.is_empty() {
                // Build a SearchResults for the scorer.
                let mut hits: Vec<crate::relevance::SearchHit> = Vec::with_capacity(results.len());
                for r in &results {
                    let line = r.get("text").and_then(|t| t.as_str()).unwrap_or("");
                    let path = r.get("file").and_then(|p| p.as_str()).unwrap_or("");
                    let line_number = r.get("line").and_then(|n| n.as_u64()).unwrap_or(0);
                    hits.push(crate::relevance::SearchHit {
                        path: std::path::PathBuf::from(path),
                        line_number,
                        line: line.to_string(),
                        before: Vec::new(),
                        after: Vec::new(),
                    });
                }
                let sr = crate::relevance::SearchResults {
                    hits: hits.clone(),
                    files_searched: 0,
                    files_skipped: 0,
                };
                // Score and reorder the `results` Vec to match.
                let ranked = crate::relevance::rank_hits(&sr, pattern);
                let mut reordered: Vec<serde_json::Value> = Vec::with_capacity(results.len());
                for (path, line) in ranked {
                    if let Some(pos) = results.iter().position(|r| {
                        r.get("file").and_then(|f| f.as_str()) == Some(path.as_str())
                            && r.get("line").and_then(|n| n.as_u64()) == Some(line)
                    }) {
                        reordered.push(results[pos].clone());
                    }
                }
                // Any that the ranker dropped are appended in order.
                for r in &results {
                    if !reordered.contains(r) {
                        reordered.push(r.clone());
                    }
                }
                results = reordered;
            }
        }


        Ok(ToolResult::Success(serde_json::json!({
            "pattern": pattern,
            "case_insensitive": case_insensitive,
            "results": results,
            "truncated": results.len() >= MAX_GREP_MATCHES,
            "skipped_large_files": skipped_large_files,
            "skipped_large_files_total": skipped_large_total,
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

/// Tier 1.3 — sanitize a read_file payload.
fn redact_read_content(content: &str, context: &ToolContext, resolved: &std::path::Path) -> String {
    let Some(redactor) = context.redactor.as_ref() else {
        return content.to_string();
    };
    let Some(rp) = context.read_protection.as_ref() else {
        return content.to_string();
    };
    if !rp.matches(resolved) {
        return content.to_string();
    }
    let (sanitized, events) = redactor.redact(content);
    if events.is_empty() {
        return sanitized;
    }
    let summary = events
        .iter()
        .map(|e| format!("{}×{}", e.rule, e.count))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "// ⓘ {} secret(s) redacted ({summary}).\n{}",
        events.iter().map(|e| e.count).sum::<usize>(),
        sanitized,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use kod_types::ToolPermissions;

    fn full_context(dir: &std::path::Path) -> ToolContext {
        // These tests exercise the raw shell path — the sandbox wraps
        // the command in bwrap / sandbox-exec on hosts where one is
        // available, changing stdout, timing, and truncation. Opt out
        // explicitly; the sandbox itself is exercised by its own
        // integration tests.
        ToolContext::new(dir)
            .with_permissions(ToolPermissions {
                read_files: true,
                write_files: true,
                execute_commands: true,
                ..Default::default()
            })
            .with_sandbox(crate::context::SandboxMode::Disabled)
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

    /// A file with NUL bytes in the first 1 KB must be returned as
    /// binary (flag + hex preview), not decoded as UTF-8 lossy.
    /// Regression: the previous code passed the bytes through
    /// from_utf8_lossy, so the model saw a wall of U+FFFD characters
    /// and had no way to know the file was a PNG, an ELF, or a
    /// UTF-16 text file.
    /// Writing under a path whose parent is a file (not a directory)
    /// must return Ok(ToolResult::Error(...)) with a message the model
    /// can act on — the same shape read_file uses for directories and
    /// missing files. Regression: the previous code propagated
    /// KodError::Io, forcing callers to handle two shapes for the same
    /// class of "the model asked for something that cannot work."
    #[tokio::test]
    async fn test_write_file_parent_is_a_file_reports_error() {
        let temp = tempfile::TempDir::new().unwrap();
        // A file named "blocker" — the write target below has it as a
        // parent directory, which create_dir_all will reject.
        std::fs::write(temp.path().join("blocker"), "not a dir").unwrap();

        let ctx = full_context(temp.path());
        let tool = WriteFileTool::new();
        let params = serde_json::json!({
            "path": "blocker/child.txt",
            "content": "hello"
        });
        let result = tool.execute(&params, &ctx).await.unwrap();

        match result {
            ToolResult::Error(msg) => {
                assert!(msg.contains("blocker"), "error should name the path: {msg}");
            }
            other => panic!("expected ToolResult::Error, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn read_file_detects_binary_content() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("img.png");
        // PNG magic + a few NUL-containing bytes.
        let bytes: Vec<u8> = vec![
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, // PNG signature
            0x00, 0x00, 0x00, 0x0D, // IHDR length
            0x49, 0x48, 0x44, 0x52, // "IHDR"
        ];
        std::fs::write(&path, &bytes).unwrap();

        let ctx = full_context(temp.path());
        let tool = ReadFileTool::new();
        let params = serde_json::json!({ "path": "img.png" });
        let result = tool.execute(&params, &ctx).await.unwrap();

        match result {
            ToolResult::Success(v) => {
                assert_eq!(v["binary"], true, "binary flag should be set");
                assert_eq!(v["size_bytes"], bytes.len() as u64);
                assert!(v.get("content").is_none(), "no text content field");
                let hex = v["preview_hex"].as_str().unwrap();
                assert!(
                    hex.starts_with("89 50 4e 47"),
                    "hex preview should start with the PNG signature: {hex}"
                );
            }
            other => panic!("expected success, got {:?}", other),
        }
    }

    /// A plain text file must be returned as text with binary: false.
    #[tokio::test]
    async fn read_file_text_file_reports_not_binary() {
        let temp = tempfile::TempDir::new().unwrap();
        std::fs::write(temp.path().join("code.rs"), "fn main() {}\n").unwrap();

        let ctx = full_context(temp.path());
        let tool = ReadFileTool::new();
        let params = serde_json::json!({ "path": "code.rs" });
        let result = tool.execute(&params, &ctx).await.unwrap();

        match result {
            ToolResult::Success(v) => {
                assert_eq!(v["binary"], false);
                assert_eq!(v["content"], "fn main() {}\n");
            }
            other => panic!("expected success, got {:?}", other),
        }
    }

    /// A UTF-8 file with non-ASCII content (accented letters, emoji)
    /// must NOT be misclassified as binary. The heuristic is NUL
    /// bytes, not "any non-ASCII".
    #[tokio::test]
    async fn read_file_multibyte_utf8_is_not_binary() {
        let temp = tempfile::TempDir::new().unwrap();
        std::fs::write(temp.path().join("accented.txt"), "café au lait — un été\n").unwrap();

        let ctx = full_context(temp.path());
        let tool = ReadFileTool::new();
        let params = serde_json::json!({ "path": "accented.txt" });
        let result = tool.execute(&params, &ctx).await.unwrap();

        match result {
            ToolResult::Success(v) => {
                assert_eq!(v["binary"], false);
                assert!(v["content"].as_str().unwrap().contains("café"));
            }
            other => panic!("expected success, got {:?}", other),
        }
    }

    /// read_file on a directory must return an error message the
    /// model can act on ("use list_files"), not a raw "Is a directory"
    /// IO error that it cannot distinguish from "file not found".
    #[tokio::test]
    async fn read_file_directory_returns_actionable_error() {
        let temp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(temp.path().join("subdir")).unwrap();
        std::fs::write(temp.path().join("subdir/x.txt"), "x").unwrap();

        let ctx = full_context(temp.path());
        let tool = ReadFileTool::new();
        let params = serde_json::json!({ "path": "subdir" });
        let result = tool.execute(&params, &ctx).await.unwrap();

        match result {
            ToolResult::Error(msg) => {
                assert!(
                    msg.contains("is a directory"),
                    "message should name the problem: {msg}"
                );
                assert!(
                    msg.contains("list_files"),
                    "message should suggest the fix: {msg}"
                );
            }
            other => panic!("expected ToolResult::Error, got {:?}", other),
        }
    }

    /// read_file on a missing path must say so, and the message must
    /// suggest checking the path rather than re-running the same call.
    #[tokio::test]
    async fn read_file_missing_returns_actionable_error() {
        let temp = tempfile::TempDir::new().unwrap();
        let ctx = full_context(temp.path());
        let tool = ReadFileTool::new();
        let params = serde_json::json!({ "path": "nope.txt" });
        let result = tool.execute(&params, &ctx).await.unwrap();

        match result {
            ToolResult::Error(msg) => {
                assert!(
                    msg.contains("not found"),
                    "message should say not-found: {msg}"
                );
                assert!(
                    msg.contains("nope.txt"),
                    "message should name the path: {msg}"
                );
            }
            other => panic!("expected ToolResult::Error, got {:?}", other),
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

    /// A command that produces no output and never exits must be
    /// killed at `context.timeout_secs` — without panicking.
    ///
    /// Regression: the previous select! loop gated all three branches
    /// on `!timed_out`. When the timeout branch set `timed_out =
    /// true`, the next loop iteration reached `tokio::select!` with
    /// every branch guard false and no `else`, which panics with
    /// "all branches are disabled and there is no else branch". The
    /// runaway-output test used `yes` and never hit this path.
    #[cfg(unix)]
    #[tokio::test]
    async fn execute_command_times_out_without_panic() {
        let temp = tempfile::TempDir::new().unwrap();
        // 1-second timeout so the test runs quickly.
        let ctx = ToolContext::new(temp.path())
            .with_permissions(kod_types::ToolPermissions {
                execute_commands: true,
                ..Default::default()
            })
            .with_timeout(1);
        let tool = ExecuteCommandTool::new();
        // `sleep 60` produces no output and exits only when it
        // finishes. The timeout is the only thing that ends it here.
        let params = serde_json::json!({ "command": "sleep 60" });

        let start = std::time::Instant::now();
        let result = tool.execute(&params, &ctx).await.unwrap();
        let elapsed = start.elapsed();

        // Generous upper bound: 1s timeout + process-kill overhead.
        // A panic would fail the test before this assertion; a
        // missed timeout would blow past it.
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "execute_command returned after {elapsed:?} — timeout of 1s was not enforced"
        );
        match result {
            ToolResult::Success(v) => {
                assert_eq!(v["timed_out"], true, "result must report timed_out: {v}");
                assert_eq!(v["timeout_secs"], 1);
                // Nothing on stdout — sleep produces no output.
                assert_eq!(v["stdout"], "");
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

    /// A file larger than MAX_GREP_FILE_BYTES must be skipped, and
    /// the skip must be reported in the result. Before streaming, the
    /// old code called read_to_string on every candidate file and
    /// would OOM on a multi-gigabyte log.
    #[tokio::test]
    async fn grep_skips_oversized_files() {
        let temp = tempfile::TempDir::new().unwrap();
        // A file one byte over the cap. Filling it with 'x' is fast
        // enough; the pattern will not match anyway, so the skip path
        // is the only thing that determines the outcome.
        let big = temp.path().join("huge.log");
        let filler = vec![b'x'; (MAX_GREP_FILE_BYTES + 1) as usize];
        std::fs::write(&big, &filler).unwrap();
        // A small file that would match, so we can also assert the
        // search continued to the small file after the skip.
        std::fs::write(temp.path().join("small.txt"), "needle here").unwrap();

        let ctx = grep_ctx(temp.path());
        let tool = GrepTool::new();
        let params = serde_json::json!({ "path": ".", "pattern": "needle" });
        let result = tool.execute(&params, &ctx).await.unwrap();

        match result {
            ToolResult::Success(v) => {
                let hits = v["results"].as_array().unwrap();
                assert_eq!(hits.len(), 1, "small file match missing: {hits:?}");
                let skipped = v["skipped_large_files"].as_array().unwrap();
                assert_eq!(skipped.len(), 1, "huge.log should be skipped");
                assert!(skipped[0].as_str().unwrap().ends_with("huge.log"));
            }
            other => panic!("expected success, got {:?}", other),
        }
    }

    /// A file within the cap must be searched normally even when there
    /// is also a skipped file. Sanity that the `continue` after the
    /// size check does not accidentally short-circuit the walk.
    #[tokio::test]
    async fn grep_searches_files_within_cap() {
        let temp = tempfile::TempDir::new().unwrap();
        std::fs::write(temp.path().join("a.txt"), "alpha\nneedle\n").unwrap();
        std::fs::write(temp.path().join("b.txt"), "beta\nneedle too\n").unwrap();

        let ctx = grep_ctx(temp.path());
        let tool = GrepTool::new();
        let params = serde_json::json!({ "path": ".", "pattern": "needle" });
        let result = tool.execute(&params, &ctx).await.unwrap();

        match result {
            ToolResult::Success(v) => {
                assert_eq!(v["results"].as_array().unwrap().len(), 2);
                assert!(
                    v["skipped_large_files"].as_array().unwrap().is_empty(),
                    "nothing should be skipped at this size"
                );
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

    /// list_files on a file must return path_kind: "file" with a
    /// single-entry list, not the ambiguous empty-directory shape.
    /// Regression: the walker rooted at a file yielded nothing, so the
    /// result was indistinguishable from an empty directory.
    #[tokio::test]
    async fn list_files_on_file_reports_file_kind() {
        let temp = tempfile::TempDir::new().unwrap();
        std::fs::write(temp.path().join("single.txt"), "content").unwrap();

        let ctx = full_context(temp.path());
        let tool = ListFilesTool::new();
        let params = serde_json::json!({ "path": "single.txt" });
        let result = tool.execute(&params, &ctx).await.unwrap();

        match result {
            ToolResult::Success(v) => {
                assert_eq!(v["path_kind"], "file");
                let files = v["files"].as_array().unwrap();
                assert_eq!(files.len(), 1, "single-entry list expected");
                assert!(files[0].as_str().unwrap().ends_with("single.txt"));
                assert_eq!(v["total"], 1);
                assert_eq!(v["truncated"], false);
            }
            other => panic!("expected success, got {:?}", other),
        }
    }

    /// list_files on a directory still reports directory and its
    /// entries, unchanged.
    #[tokio::test]
    async fn list_files_on_directory_reports_directory_kind() {
        let temp = tempfile::TempDir::new().unwrap();
        std::fs::write(temp.path().join("a.txt"), "").unwrap();
        std::fs::write(temp.path().join("b.txt"), "").unwrap();

        let ctx = full_context(temp.path());
        let tool = ListFilesTool::new();
        let params = serde_json::json!({ "path": "." });
        let result = tool.execute(&params, &ctx).await.unwrap();

        match result {
            ToolResult::Success(v) => {
                assert_eq!(v["path_kind"], "directory");
                assert_eq!(v["files"].as_array().unwrap().len(), 2);
                assert_eq!(v["total"], 2);
            }
            other => panic!("expected success, got {:?}", other),
        }
    }

    /// list_files on a missing path returns a structured error naming
    /// the path, not an empty result.
    #[tokio::test]
    async fn list_files_missing_path_errors() {
        let temp = tempfile::TempDir::new().unwrap();
        let ctx = full_context(temp.path());
        let tool = ListFilesTool::new();
        let params = serde_json::json!({ "path": "does-not-exist" });
        let result = tool.execute(&params, &ctx).await.unwrap();

        match result {
            ToolResult::Error(msg) => {
                assert!(
                    msg.contains("not found"),
                    "message should say not-found: {msg}"
                );
                assert!(msg.contains("does-not-exist"));
            }
            other => panic!("expected ToolResult::Error, got {:?}", other),
        }
    }
}

#[cfg(test)]
mod coverage_execute_command_env {
    //! H-S5 regression suite. The child process must not see
    //! credential-shaped env vars. The exact set is a policy, not a
    //! contract — the tests pin the *shape* (an obvious key is
    //! stripped; an obvious path is kept), so a future expansion of
    //! the matcher does not silently regress the "secrets never
    //! reach a subprocess" invariant.
    use super::is_secret_like_env;

    #[test]
    fn well_known_credentials_are_stripped() {
        for name in [
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "TYPESAFE_API_KEY",
            "AWS_SECRET_ACCESS_KEY",
            "GOOGLE_APPLICATION_CREDENTIALS",
            "AZURE_CLIENT_SECRET",
            "GITHUB_TOKEN",
            "MY_PASSWORD",
            "SOME_BEARER_VALUE",
            "CUSTOM_API_TOKEN",
        ] {
            assert!(is_secret_like_env(name), "{name} should be stripped");
        }
    }

    #[test]
    fn shell_furniture_is_kept() {
        for name in [
            "PATH", "HOME", "USER", "LOGNAME", "SHELL", "TERM", "PWD", "OLDPWD", "LANG", "TMPDIR",
            "TMP", "TEMP",
        ] {
            assert!(!is_secret_like_env(name), "{name} must be kept");
        }
    }

    #[test]
    fn locale_variants_are_kept() {
        for name in ["LC_ALL", "LC_CTYPE", "LC_MESSAGES", "LC_TIME"] {
            assert!(!is_secret_like_env(name), "{name} must be kept");
        }
    }

    #[test]
    fn ordinary_names_are_kept() {
        for name in ["CARGO_HOME", "RUSTUP_HOME", "EDITOR", "PAGER", "COLORTERM"] {
            assert!(!is_secret_like_env(name), "{name} must be kept");
        }
    }

    #[test]
    fn matching_is_case_insensitive() {
        assert!(is_secret_like_env("anthropic_api_key"));
        assert!(is_secret_like_env("Some_Token"));
    }
}
