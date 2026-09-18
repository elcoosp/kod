//! Minimal LSP client: JSON-RPC over stdio with `Content-Length`
//! framing.
//!
//! # Scope
//!
//! This is a diagnostic-only client. It:
//!
//! 1. spawns a server process,
//! 2. sends `initialize` / `initialized`,
//! 3. sends `textDocument/didOpen` for one file,
//! 4. reads `textDocument/publishDiagnostics` notifications until it
//!    has seen the file's diagnostics settle,
//! 5. sends `shutdown` / `exit`.
//!
//! Any LSP method the server sends to us as a *request* (like
//! `client/registerCapability` or `window/workDoneProgress/create`)
//! is answered with `{"result": null}` — the standard "I don't
//! support this" reply. That is what makes a minimal client work in
//! practice: the server does not need us to understand every
//! capability it offers, only to acknowledge the ones it asks about.
//!
//! # What this is not
//!
//! Not a persistent client. Each `LspClient` owns one child process
//! and shuts it down on drop. Caching the process across calls (so
//! `didChange` is cheap) is a follow-up; today the cost is one spawn
//! per `check`.

use crate::types::Diagnostic;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

#[derive(Debug, thiserror::Error)]
pub enum LspError {
    #[error("could not spawn {program}: {source}")]
    Spawn {
        program: String,
        source: std::io::Error,
    },
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("protocol: {0}")]
    Protocol(String),
    #[error("timeout waiting for server response")]
    Timeout,
}

/// A running language server.
pub struct LspClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
    workspace_root: PathBuf,
    /// The program we spawned — kept for logging and for
    /// `CheckOutcome.command`.
    program: String,
    /// Files we have sent `textDocument/didOpen` for, mapped to their
    /// last `didChange` version. LSP requires one open per document
    /// followed by changes; re-opening an already-open file is a
    /// protocol error. Keyed by canonical path so equivalent spellings
    /// (`./foo.rs`, `foo.rs`, absolute) resolve to the same entry.
    opened: std::collections::HashMap<PathBuf, i64>,
}

impl LspClient {
    /// Spawn a server. Does not yet send `initialize`; call
    /// [`LspClient::initialize`] before any other request.
    pub async fn start(program: &str, workspace_root: &Path) -> Result<Self, LspError> {
        let mut child = Command::new(program)
            .current_dir(workspace_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Keep stderr quiet by default. A future version could
            // pipe it and surface `window/logMessage` from it, but the
            // protocol already carries what we need.
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|source| LspError::Spawn {
                program: program.to_string(),
                source,
            })?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| LspError::Protocol("child has no stdin".to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| LspError::Protocol("child has no stdout".to_string()))?;

        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 1,
            workspace_root: workspace_root.to_path_buf(),
            program: program.to_string(),
            opened: std::collections::HashMap::new(),
        })
    }

    pub fn program(&self) -> &str {
        &self.program
    }

    /// Send `initialize`, wait for the matching response, send
    /// `initialized`. After this returns the server is ready.
    pub async fn initialize(&mut self) -> Result<(), LspError> {
        let root_uri = path_to_uri(&self.workspace_root);
        let id = self
            .request(
                "initialize",
                serde_json::json!({
                    "processId": std::process::id(),
                    "rootUri": root_uri,
                    "capabilities": {
                        "textDocument": {
                            // The only client capability we actually
                            // use: we want the server to publish
                            // diagnostics for files we open.
                            "publishDiagnostics": {}
                        }
                    },
                    "workspaceFolders": [{
                        "uri": root_uri,
                        "name": "workspace"
                    }]
                }),
            )
            .await?;

        loop {
            let msg = self.read_handling_server_requests().await?;
            if msg.get("id").and_then(|v| v.as_i64()) == Some(id) {
                if let Some(err) = msg.get("error") {
                    return Err(LspError::Protocol(format!(
                        "initialize returned an error: {err}"
                    )));
                }
                break;
            }
        }

        self.notify("initialized", serde_json::json!({})).await?;
        Ok(())
    }

    /// Open a file. The server will analyze it and send
    /// `publishDiagnostics` notifications.
    pub async fn did_open(&mut self, path: &Path, content: &str) -> Result<(), LspError> {
        let uri = path_to_uri(path);
        let language_id = crate::types::language_id_for(path);
        self.notify(
            "textDocument/didOpen",
            serde_json::json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": language_id,
                    "version": 1,
                    "text": content,
                }
            }),
        )
        .await
    }

    /// Notify the server that a file changed. Only valid after
    /// [`LspClient::did_open`] for the same file.
    pub async fn did_change(
        &mut self,
        path: &Path,
        content: &str,
        version: i64,
    ) -> Result<(), LspError> {
        let uri = path_to_uri(path);
        self.notify(
            "textDocument/didChange",
            serde_json::json!({
                "textDocument": { "uri": uri, "version": version },
                "contentChanges": [{ "text": content }]
            }),
        )
        .await
    }

    /// Get diagnostics for `path`, opening the file on the first call
    /// and sending an incremental change on the rest.
    ///
    /// This is the method a caller should use; `did_open` and
    /// `did_change` are the raw protocol steps and exist mainly for
    /// tests. The version number is bumped internally so LSP's
    /// monotonicity requirement holds without the caller tracking it.
    pub async fn diagnostics(
        &mut self,
        path: &Path,
        content: &str,
        overall_timeout: Duration,
    ) -> Result<Vec<Diagnostic>, LspError> {
        let key = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        match self.opened.get_mut(&key) {
            Some(version) => {
                *version += 1;
                let v = *version;
                self.did_change(&key, content, v).await?;
            }
            None => {
                self.did_open(&key, content).await?;
                self.opened.insert(key.clone(), 1);
            }
        }
        self.collect_diagnostics(&key, overall_timeout).await
    }

    /// Read notifications until diagnostics for `path` have arrived
    /// and settled. "Settled" is: at least one
    /// `textDocument/publishDiagnostics` for this file has been seen,
    /// and no new message has arrived for [`SETTLE_AFTER`]. That is
    /// what rust-analyzer does in practice — it publishes an empty
    /// list first, then the real list once indexing finishes — and a
    /// fixed post-first-message wait is the simplest correct handling.
    ///
    /// `overall_timeout` caps the total wait regardless.
    pub async fn collect_diagnostics(
        &mut self,
        path: &Path,
        overall_timeout: Duration,
    ) -> Result<Vec<Diagnostic>, LspError> {
        const SETTLE_AFTER: Duration = Duration::from_millis(800);
        let target_uri = path_to_uri(path);
        let deadline = tokio::time::Instant::now() + overall_timeout;
        let mut latest: Option<Vec<Diagnostic>> = None;
        let mut last_diag_at: Option<tokio::time::Instant> = None;

        loop {
            let now = tokio::time::Instant::now();
            if now >= deadline {
                break;
            }
            if let Some(t) = last_diag_at
                && t.elapsed() >= SETTLE_AFTER
            {
                break;
            }
            let remaining = deadline.saturating_duration_since(now);
            let read = tokio::time::timeout(remaining, self.read_handling_server_requests()).await;
            let msg = match read {
                Ok(Ok(m)) => m,
                Ok(Err(LspError::Io(e))) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    break;
                }
                Ok(Err(e)) => return Err(e),
                Err(_) => break, // overall timeout
            };
            let is_our_diags = msg.get("method").and_then(|v| v.as_str())
                == Some("textDocument/publishDiagnostics")
                && msg
                    .get("params")
                    .and_then(|p| p.get("uri"))
                    .and_then(|v| v.as_str())
                    == Some(&target_uri);
            if is_our_diags && let Some(params) = msg.get("params") {
                latest = Some(parse_diagnostics(params, &target_uri));
                last_diag_at = Some(tokio::time::Instant::now());
            }
        }

        Ok(latest.unwrap_or_default())
    }

    /// `textDocument/definition` at `pos`. Sends the request, waits
    /// for the response (30 s timeout), and parses the Location(s)
    /// the server returns. LSP allows a single Location, an array of
    /// Locations, or `null`; all three shapes are normalised here.
    pub async fn definition(
        &mut self,
        path: &std::path::Path,
        pos: crate::types::Position,
    ) -> Result<Vec<crate::types::Location>, LspError> {
        self.ensure_open(path).await?;
        let uri = path_to_uri(path);
        let result = self
            .request_with_response(
                "textDocument/definition",
                serde_json::json!({
                    "textDocument": { "uri": uri },
                    "position": {
                        "line": pos.line.saturating_sub(1),
                        "character": pos.column.saturating_sub(1),
                    },
                }),
            )
            .await?;
        Ok(parse_locations(&result))
    }

    /// `textDocument/references`. `include_declaration` controls
    /// whether the symbol's own declaration is included; true is the
    /// usual choice for a coding agent (it wants every mention).
    pub async fn references(
        &mut self,
        path: &std::path::Path,
        pos: crate::types::Position,
        include_declaration: bool,
    ) -> Result<Vec<crate::types::Location>, LspError> {
        self.ensure_open(path).await?;
        let uri = path_to_uri(path);
        let result = self
            .request_with_response(
                "textDocument/references",
                serde_json::json!({
                    "textDocument": { "uri": uri },
                    "position": {
                        "line": pos.line.saturating_sub(1),
                        "character": pos.column.saturating_sub(1),
                    },
                    "context": { "includeDeclaration": include_declaration },
                }),
            )
            .await?;
        Ok(parse_locations(&result))
    }

    /// `textDocument/hover`. Returns the text plus the range it
    /// applies to. A `null` response is normalised to an empty Hover.
    pub async fn hover(
        &mut self,
        path: &std::path::Path,
        pos: crate::types::Position,
    ) -> Result<crate::types::Hover, LspError> {
        self.ensure_open(path).await?;
        let uri = path_to_uri(path);
        let result = self
            .request_with_response(
                "textDocument/hover",
                serde_json::json!({
                    "textDocument": { "uri": uri },
                    "position": {
                        "line": pos.line.saturating_sub(1),
                        "character": pos.column.saturating_sub(1),
                    },
                }),
            )
            .await?;
        Ok(parse_hover(&result))
    }

    /// Ensure `path` has been sent to the server via `didOpen`. A
    /// second call for the same path is a no-op — the server is
    /// already tracking the document. A fresh file is sent with an
    /// empty body; hover/definition/references do not need the
    /// current content to answer (they need the *on-disk* file, which
    /// the server reads itself).
    ///
    /// `diagnostics` is separate: it wants the exact current content
    /// because it is asking the server to check a write the model
    /// just made.
    async fn ensure_open(&mut self, path: &std::path::Path) -> Result<(), LspError> {
        let key = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        if self.opened.contains_key(&key) {
            return Ok(());
        }
        self.did_open(path, "").await?;
        self.opened.insert(key, 1);
        Ok(())
    }

    /// Send a request and return the response's `result` field. A
    /// JSON-RPC error becomes an `LspError::Protocol`.
    async fn request_with_response(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, LspError> {
        let id = self.next_id;
        self.next_id += 1;
        self.send_message(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .await?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return Err(LspError::Timeout);
            }
            let remaining = deadline - now;
            let msg = tokio::time::timeout(remaining, self.read_handling_server_requests())
                .await
                .map_err(|_| LspError::Timeout)??;
            if msg.get("id").and_then(|v| v.as_i64()) == Some(id) {
                if let Some(err) = msg.get("error") {
                    return Err(LspError::Protocol(format!(
                        "{method} returned an error: {err}"
                    )));
                }
                return Ok(msg
                    .get("result")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null));
            }
        }
    }

    /// Best-effort graceful shutdown. Errors are ignored — the process
    /// is about to die regardless, and `kill_on_drop` is the safety
    /// net.
    pub async fn shutdown(mut self) {
        let _ = self.request("shutdown", serde_json::json!(null)).await;
        let _ = self.notify("exit", serde_json::json!(null)).await;
        let _ = tokio::time::timeout(Duration::from_secs(2), self.child.wait()).await;
    }

    // ---------------------------------------------------------------------
    // Protocol plumbing
    // ---------------------------------------------------------------------

    async fn request(&mut self, method: &str, params: serde_json::Value) -> Result<i64, LspError> {
        let id = self.next_id;
        self.next_id += 1;
        self.send_message(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .await?;
        Ok(id)
    }

    async fn notify(&mut self, method: &str, params: serde_json::Value) -> Result<(), LspError> {
        self.send_message(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))
        .await
    }

    async fn send_message(&mut self, msg: &serde_json::Value) -> Result<(), LspError> {
        let body = serde_json::to_vec(msg)?;
        let header = format!("Content-Length: {}\r\n\r\n", body.len());
        self.stdin.write_all(header.as_bytes()).await?;
        self.stdin.write_all(&body).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    /// Read one full message. A server-to-client *request* (has both
    /// `id` and `method`) is answered with `{"result": null}` and the
    /// loop continues; a notification or a response to one of our
    /// requests is returned to the caller.
    async fn read_handling_server_requests(&mut self) -> Result<serde_json::Value, LspError> {
        loop {
            let msg = self.read_message().await?;
            if let (Some(id), Some(_method)) = (
                msg.get("id").and_then(|v| v.as_i64()),
                msg.get("method").and_then(|v| v.as_str()),
            ) {
                self.send_message(&serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": serde_json::Value::Null,
                }))
                .await?;
                continue;
            }
            return Ok(msg);
        }
    }

    async fn read_message(&mut self) -> Result<serde_json::Value, LspError> {
        // Headers: `Content-Length: N\r\n` possibly with others, then
        // a blank line.
        let mut content_length: Option<usize> = None;
        loop {
            let mut line = String::new();
            let n = self.stdout.read_line(&mut line).await?;
            if n == 0 {
                return Err(LspError::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "server closed stdout",
                )));
            }
            let trimmed = line.trim_end_matches(['\r', '\n']);
            if trimmed.is_empty() {
                break;
            }
            if let Some(rest) = trimmed.strip_prefix("Content-Length:") {
                content_length =
                    Some(rest.trim().parse().map_err(|_| {
                        LspError::Protocol(format!("bad Content-Length: {rest:?}"))
                    })?);
            }
        }
        let n = content_length
            .ok_or_else(|| LspError::Protocol("missing Content-Length header".to_string()))?;
        let mut buf = vec![0u8; n];
        self.stdout.read_exact(&mut buf).await?;
        Ok(serde_json::from_slice(&buf)?)
    }
}

/// `file://` URI for a path. Absolute, canonical when possible.
fn path_to_uri(p: &Path) -> String {
    let abs = std::fs::canonicalize(p).unwrap_or_else(|_| {
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            std::env::current_dir()
                .map(|c| c.join(p))
                .unwrap_or_else(|_| p.to_path_buf())
        }
    });
    let s = abs.to_string_lossy();
    // Unix: `/foo` → `file:///foo`. Windows: `C:\foo` → `file:///C:/foo`.
    if s.starts_with('/') {
        format!("file://{s}")
    } else {
        format!("file:///{}", s.replace('\\', "/"))
    }
}

/// Parse a `publishDiagnostics` params object into our `Diagnostic`
/// shape. Lines and columns are converted from LSP's 0-based to
/// 1-based.
fn parse_diagnostics(params: &serde_json::Value, uri: &str) -> Vec<Diagnostic> {
    let file = uri.strip_prefix("file://").unwrap_or(uri).to_string();
    let Some(arr) = params.get("diagnostics").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|d| {
            let start = d.get("range")?.get("start")?;
            let line = start.get("line")?.as_u64()? + 1;
            let character = start.get("character")?.as_u64()? + 1;
            let severity_n = d.get("severity").and_then(|v| v.as_u64()).unwrap_or(1);
            let severity = match severity_n {
                1 => "error",
                2 => "warning",
                3 => "info",
                _ => "hint",
            };
            // The `code` field can be a string or a number; normalize
            // to a string.
            let code = d.get("code").and_then(|v| {
                v.as_str()
                    .map(String::from)
                    .or_else(|| v.as_i64().map(|n| n.to_string()))
            });
            let message = d.get("message")?.as_str()?.to_string();
            Some(Diagnostic {
                file: file.clone(),
                line: line as u32,
                column: character as u32,
                severity: severity.to_string(),
                code,
                message,
            })
        })
        .collect()
}

/// Parse the response of `textDocument/definition` /
/// `textDocument/references` into a `Vec<Location>`. Handles the
/// three LSP-legal shapes: a single Location, an array of Locations,
/// or `null`.
fn parse_locations(v: &serde_json::Value) -> Vec<crate::types::Location> {
    fn to_pos(p: &serde_json::Value) -> Option<crate::types::Position> {
        Some(crate::types::Position {
            line: (p.get("line")?.as_u64()? + 1) as u32,
            column: (p.get("character")?.as_u64()? + 1) as u32,
        })
    }
    fn one(item: &serde_json::Value) -> Option<crate::types::Location> {
        let uri = item.get("uri")?.as_str()?;
        let file = uri_to_path(uri);
        let r = item.get("range")?;
        let start = r.get("start")?;
        let end = r.get("end")?;
        Some(crate::types::Location {
            file,
            range: crate::types::Range {
                start: to_pos(start)?,
                end: to_pos(end)?,
            },
        })
    }
    if v.is_null() {
        return Vec::new();
    }
    if let Some(arr) = v.as_array() {
        arr.iter().filter_map(one).collect()
    } else {
        one(v).into_iter().collect()
    }
}

/// Parse a `textDocument/hover` response.
fn parse_hover(v: &serde_json::Value) -> crate::types::Hover {
    if v.is_null() {
        return crate::types::Hover {
            text: String::new(),
            range: None,
        };
    }
    fn to_pos(p: &serde_json::Value) -> Option<crate::types::Position> {
        Some(crate::types::Position {
            line: (p.get("line")?.as_u64()? + 1) as u32,
            column: (p.get("character")?.as_u64()? + 1) as u32,
        })
    }
    let range = v.get("range").and_then(|r| {
        let s = r.get("start")?;
        let e = r.get("end")?;
        Some(crate::types::Range {
            start: to_pos(s)?,
            end: to_pos(e)?,
        })
    });
    // `contents` is MarkupContent, MarkedString, or an array of
    // either. We flatten all of them into a single string.
    let text = match v.get("contents") {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .filter_map(|it| match it {
                serde_json::Value::String(s) => Some(s.clone()),
                serde_json::Value::Object(_) => {
                    it.get("value").and_then(|x| x.as_str()).map(String::from)
                }
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Some(serde_json::Value::Object(_)) => v
            .get("contents")
            .and_then(|c| c.get("value"))
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        _ => String::new(),
    };
    crate::types::Hover { text, range }
}

/// Inverse of `path_to_uri` for the shapes we produce and consume:
/// `file:///abs/path` on Unix, `file:///C:/path` on Windows.
fn uri_to_path(uri: &str) -> std::path::PathBuf {
    let rest = uri.strip_prefix("file://").unwrap_or(uri);
    // Strip a leading slash on Windows (file:///C:/...) but keep it
    // on Unix (/abs/path).
    #[cfg(windows)]
    {
        let s = rest.trim_start_matches('/');
        std::path::PathBuf::from(s.replace('/', "\\"))
    }
    #[cfg(not(windows))]
    {
        std::path::PathBuf::from(rest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `true` if `name` is an executable file on PATH. Used by the
    /// `#[ignore]`d live tests to skip cleanly when a language server
    /// is not installed, rather than failing with a spawn error.
    fn binary_on_path(name: &str) -> bool {
        std::env::var_os("PATH")
            .map(|path| std::env::split_paths(&path).any(|d| d.join(name).is_file()))
            .unwrap_or(false)
    }

    #[test]
    fn path_to_uri_unix() {
        assert_eq!(path_to_uri(Path::new("/tmp/x.rs")), "file:///tmp/x.rs");
    }

    #[test]
    fn parse_diagnostics_empty() {
        let params = serde_json::json!({
            "uri": "file:///tmp/x.rs",
            "diagnostics": []
        });
        let out = parse_diagnostics(&params, "file:///tmp/x.rs");
        assert!(out.is_empty());
    }

    #[test]
    fn parse_diagnostics_one_error() {
        let params = serde_json::json!({
            "uri": "file:///tmp/x.rs",
            "diagnostics": [{
                "range": {
                    "start": { "line": 41, "character": 4 },
                    "end":   { "line": 41, "character": 10 }
                },
                "severity": 1,
                "code": "E0308",
                "message": "mismatched types"
            }]
        });
        let out = parse_diagnostics(&params, "file:///tmp/x.rs");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].line, 42); // 0-based 41 → 1-based 42
        assert_eq!(out[0].column, 5);
        assert_eq!(out[0].severity, "error");
        assert_eq!(out[0].code.as_deref(), Some("E0308"));
        assert_eq!(out[0].message, "mismatched types");
    }

    #[test]
    fn parse_diagnostics_numeric_code_becomes_string() {
        let params = serde_json::json!({
            "uri": "file:///tmp/x.rs",
            "diagnostics": [{
                "range": { "start": { "line": 0, "character": 0 } },
                "severity": 2,
                "code": 2322,
                "message": "type mismatch"
            }]
        });
        let out = parse_diagnostics(&params, "file:///tmp/x.rs");
        assert_eq!(out[0].code.as_deref(), Some("2322"));
    }

    /// Second call on the same file must send `didChange`, not
    /// `didOpen`. `LspClient::diagnostics` handles the distinction;
    /// this test drives it end to end and asserts the server's
    /// response reflects the new content.
    ///
    /// The sequence:
    ///
    /// 1. Write a file with a type error.
    /// 2. `diagnostics` → expect the error.
    /// 3. Rewrite the file with the error fixed.
    /// 4. `diagnostics` again → expect no error.
    ///
    /// If step 4 sends a second `didOpen`, rust-analyzer logs
    /// "document already open" and returns the *stale* diagnostics
    /// from step 2. The assertion on emptiness catches that.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_rust_analyzer_did_change_clears_error() {
        if !binary_on_path("rust-analyzer") {
            eprintln!("skipping: rust-analyzer not on PATH");
            return;
        }
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        let src = tmp.path().join("src/lib.rs");

        let bad = "pub fn f() -> u32 { \"not a number\" }\n";
        let good = "pub fn f() -> u32 { 42 }\n";
        std::fs::write(&src, bad).unwrap();

        // Real runtime guard: `binary_on_path` only proves a file named
        // `rust-analyzer` exists on `$PATH`. On a host where rustup is
        // the source of that file, it is a shim that dies at
        // `initialize` with "Unknown binary 'rust-analyzer' in official
        // toolchain". The test then fails on a machine that does not
        // actually have a working rust-analyzer — the exact failure the
        // previous `#[ignore]` was hiding. Probing the handshake is
        // the honest precondition.
        let mut client = match LspClient::start("rust-analyzer", tmp.path()).await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("skipping: could not spawn rust-analyzer: {e}");
                return;
            }
        };
        if let Err(e) = client.initialize().await {
            eprintln!(
                "skipping: rust-analyzer did not complete the LSP handshake ({e}); \
                 the binary on PATH is likely a rustup shim with no real toolchain installed"
            );
            client.shutdown().await;
            return;
        }

        // First call: didOpen. Expect a diagnostic.
        let first = client
            .diagnostics(&src, bad, Duration::from_secs(60))
            .await
            .expect("first diagnostics");
        assert!(
            !first.is_empty(),
            "first call must find the type error, got none"
        );

        // Second call: didChange. Expect the error to be gone.
        let second = client
            .diagnostics(&src, good, Duration::from_secs(30))
            .await
            .expect("second diagnostics");
        let still_erroring = second.iter().any(|d| {
            d.severity == "error"
                && (d.message.to_lowercase().contains("mismatched")
                    || d.code.as_deref() == Some("E0308"))
        });
        assert!(
            !still_erroring,
            "didChange should have cleared the error, but it persisted: {second:#?}"
        );

        client.shutdown().await;
    }
}
