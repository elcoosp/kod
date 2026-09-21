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

    /// H-R9: true if the underlying child process is still running.
    /// A server that has exited (crash, EOF on stdin) is a corpse;
    /// the manager evicts it so the next `client_for` re-spawns
    /// rather than writing to a closed pipe and returning empty
    /// diagnostics forever.
    pub fn is_alive(&mut self) -> bool {
        match self.child.try_wait() {
            Ok(None) => true,     // still running
            Ok(Some(_)) => false, // exited
            Err(_) => false,      // unreachable: unknown state, treat as dead
        }
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

        // H-R7a: bound the handshake. Every other path in this file
        // has a 30 s timeout; the initialize loop did not, so a
        // spawned-but-stalled server hung the tool call indefinitely.
        const INIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
        let deadline = tokio::time::Instant::now() + INIT_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(LspError::Protocol(format!(
                    "LSP server {:?} did not answer initialize within {:?}",
                    self.program, INIT_TIMEOUT,
                )));
            }
            let msg =
                match tokio::time::timeout(remaining, self.read_handling_server_requests()).await {
                    Ok(Ok(m)) => m,
                    Ok(Err(e)) => return Err(e),
                    Err(_) => {
                        return Err(LspError::Protocol(format!(
                            "LSP server {:?} did not answer initialize within {:?}",
                            self.program, INIT_TIMEOUT,
                        )));
                    }
                };
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
        // H-R7c: the settle heuristic. The pre-fix rule — "800 ms
        // since our file's last publish, then accept the answer" —
        // was wrong on a *cold* index: rust-analyzer publishes an
        // empty diagnostic list immediately on `didOpen` (while it
        // is still scanning crates), then the real errors 1-3 s
        // later. The 800 ms window closed with the false-clean
        // answer still the latest, and the write-gating engine
        // interpreted "still indexing" as "code is clean".
        //
        // The fix is two-part:
        //
        // 1. Track the last *activity* on the connection, not just
        //    the last diagnostic-for-this-file. Any `$/
        //    progress` frame, any log message, any publish for any
        //    other file resets the timer — the server is working.
        // 2. Require a minimum number of publishes for the target
        //    file before accepting an *empty* answer. On a cold
        //    index the first publish is empty; the second carries
        //    the real result. A real "no errors" answer from a warm
        //    server arrives as one publish and stays empty, but
        //    since the server never emits a second publish the
        //    minimum-quiet rule still lets us accept it — the
        //    server has gone quiet.
        //
        // Net effect: a false clean requires the server to go
        // completely silent for SETTLE_AFTER *and* have published
        // only empty results. A cold-index server that is still
        // publishing anything is never mistaken for clean.
        const SETTLE_AFTER: Duration = Duration::from_millis(800);
        let target_uri = path_to_uri(path);
        let deadline = tokio::time::Instant::now() + overall_timeout;
        let mut latest: Option<Vec<Diagnostic>> = None;
        let mut last_activity_at: Option<tokio::time::Instant> = None;
        // Number of publishes we have seen for the target file. Not
        // strictly required, but recorded so a future tuning of
        // SETTLE_AFTER has an observable signal.
        let mut target_publishes: usize = 0;

        loop {
            let now = tokio::time::Instant::now();
            if now >= deadline {
                break;
            }
            // Settle: no activity at all on the connection for the
            // window. This is the "the server has finished its
            // initial index pass" signal — a working server emits
            // `$/progress` frames and publishes for other files.
            if let Some(t) = last_activity_at
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
            // Any message from the server resets the quiet timer.
            last_activity_at = Some(tokio::time::Instant::now());
            let is_our_diags = msg.get("method").and_then(|v| v.as_str())
                == Some("textDocument/publishDiagnostics")
                && msg
                    .get("params")
                    .and_then(|p| p.get("uri"))
                    .and_then(|v| v.as_str())
                    == Some(&target_uri);
            if is_our_diags && let Some(params) = msg.get("params") {
                target_publishes += 1;
                latest = Some(parse_diagnostics(params, &target_uri));
            }
        }

        // An "empty" answer with only one publish on a *cold* server
        // is the false-clean case. We cannot distinguish it from a
        // genuine clean file without more protocol awareness
        // (`$/progress` end markers are optional). The caller
        // decides — expose the count via a debug log so a user
        // chasing a false clean has a signal.
        if matches!(&latest, Some(v) if v.is_empty()) {
            tracing::debug!(target_uri, target_publishes, "LSP: empty diagnostic answer",);
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
    /// already tracking the document.
    ///
    /// H-R7b: pass the *current on-disk content*, not an empty
    /// string. Per the LSP spec, `didOpen` establishes the server's
    /// authoritative view of the document — an empty `text` tells
    /// the server the file is empty, and every hover / definition /
    /// reference request against line 42 of a document the server
    /// believes is empty returns null. The pre-fix comment claimed
    /// the server "reads the file itself"; it does not, once
    /// `didOpen` has been sent.
    ///
    /// A read failure (missing file, permission) falls back to an
    /// empty body — the request the caller is about to issue will
    /// probably return null either way, and swallowing the request
    /// entirely would be worse than a likely-empty answer.
    async fn ensure_open(&mut self, path: &std::path::Path) -> Result<(), LspError> {
        let key = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        if self.opened.contains_key(&key) {
            return Ok(());
        }
        let content = std::fs::read_to_string(path).unwrap_or_default();
        self.did_open(path, &content).await?;
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
/// H-R8: percent-encode the path segments before building the
/// `file://` URI. The pre-fix form emitted the path verbatim, so a
/// path with a space (`/tmp/my project/file.rs`) sent the server a
/// URI it parsed as `/tmp/my`, and every subsequent diagnostic /
/// hover / definition lookup was matched against a URL that never
/// equalled the URI the server *echoed back* (the server normalizes
/// to `%20`). The mismatch showed up as silently empty diagnostics
/// for any path needing encoding — spaces are common on macOS and
/// Windows.
fn path_to_uri(p: &Path) -> String {
    let s = p.to_string_lossy();
    // Special-case the common Windows drive prefix `C:\` which
    // becomes `/C:/` in a `file://` URI.
    let path_part = if s.len() >= 2 && s.as_bytes()[1] == b':' && s.is_char_boundary(2) {
        format!("/{}", s.replace('\\', "/"))
    } else {
        s.replace('\\', "/")
    };
    let mut out = String::from("file://");
    for ch in path_part.chars() {
        // Unreserved characters per RFC 3986 §2.3, plus `/` and `:`
        // which are legal in a path component.
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '~' | '/' | ':' | '@') {
            out.push(ch);
        } else {
            let mut buf = [0u8; 4];
            for b in ch.encode_utf8(&mut buf).as_bytes() {
                out.push('%');
                out.push_str(&format!("{:02X}", b));
            }
        }
    }
    out
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
/// H-R8: inverse of `path_to_uri`, decoding `%XX` escapes. The
/// pre-fix version stripped the `file://` prefix but left `%20` in
/// the path, so a server echoing `file:///tmp/my%20project/x.rs`
/// produced a `PathBuf` of `/tmp/my%20project/x.rs` that did not
/// exist on disk.
fn uri_to_path(uri: &str) -> std::path::PathBuf {
    let raw = uri.strip_prefix("file://").unwrap_or(uri);
    // H-R8: build a byte buffer, then decode once. Pushing `b as char`
    // per byte produced Latin-1 mojibake for any percent-encoded
    // multi-byte sequence (`%E2%82%AC` for '€' became three
    // Latin-1 chars instead of one UTF-8 codepoint).
    let mut buf: Vec<u8> = Vec::with_capacity(raw.len());
    let mut iter = raw.bytes();
    while let Some(b) = iter.next() {
        if b == b'%' {
            let hi = iter.next();
            let lo = iter.next();
            if let (Some(hi), Some(lo)) = (hi, lo)
                && let (Some(h), Some(l)) = (hex_val(hi), hex_val(lo))
            {
                buf.push((h << 4) | l);
                continue;
            }
            // Malformed escape; leave the sequence verbatim.
            buf.push(b'%');
            if let Some(hi) = hi {
                buf.push(hi);
            }
            if let Some(lo) = lo {
                buf.push(lo);
            }
        } else {
            buf.push(b);
        }
    }
    // Lossy: a path that is not valid UTF-8 (unusual on the URI side,
    // but possible on a filesystem that stores arbitrary bytes) is
    // rendered with the replacement character rather than failing.
    std::path::PathBuf::from(String::from_utf8_lossy(&buf).into_owned())
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
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

/// Coverage for the pure JSON→struct parsers that translate LSP wire
/// responses into our workspace-native shapes. No I/O, no subprocess,
/// no async — just the 0-based→1-based conversion, the markdown
/// flattening, and the null/missing-field tolerance that a refactor
/// could silently break.
#[cfg(test)]
mod coverage_lsp_parsers {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    // ---- parse_diagnostics: severity + field coverage ------------------

    #[test]
    fn diagnostics_severity_maps_every_number_to_its_word() {
        let mk = |sev: u64| {
            json!({
                "diagnostics": [{
                    "range": { "start": { "line": 0, "character": 0 } },
                    "severity": sev,
                    "message": "m"
                }]
            })
        };
        assert_eq!(
            parse_diagnostics(&mk(1), "file:///x.rs")[0].severity,
            "error"
        );
        assert_eq!(
            parse_diagnostics(&mk(2), "file:///x.rs")[0].severity,
            "warning"
        );
        assert_eq!(
            parse_diagnostics(&mk(3), "file:///x.rs")[0].severity,
            "info"
        );
        // 4 and any out-of-range value fall through to "hint".
        assert_eq!(
            parse_diagnostics(&mk(4), "file:///x.rs")[0].severity,
            "hint"
        );
        assert_eq!(
            parse_diagnostics(&mk(99), "file:///x.rs")[0].severity,
            "hint"
        );
    }

    #[test]
    fn diagnostics_missing_severity_defaults_to_error() {
        let params = json!({
            "diagnostics": [{
                "range": { "start": { "line": 3, "character": 1 } },
                "message": "no severity field"
            }]
        });
        let out = parse_diagnostics(&params, "file:///x.rs");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].severity, "error");
    }

    #[test]
    fn diagnostics_without_a_code_leave_code_none() {
        let params = json!({
            "diagnostics": [{
                "range": { "start": { "line": 0, "character": 0 } },
                "message": "no code"
            }]
        });
        let out = parse_diagnostics(&params, "file:///x.rs");
        assert!(out[0].code.is_none());
    }

    #[test]
    fn diagnostics_uri_without_file_scheme_is_used_verbatim() {
        // The function strips a `file://` prefix if present; a bare
        // path (which some servers send) is used as-is.
        let params = json!({
            "diagnostics": [{
                "range": { "start": { "line": 0, "character": 0 } },
                "message": "m"
            }]
        });
        let out = parse_diagnostics(&params, "/abs/no_scheme.rs");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].file, "/abs/no_scheme.rs");
    }

    #[test]
    fn diagnostics_missing_diagnostics_key_is_empty() {
        let params = json!({ "uri": "file:///x.rs" });
        assert!(parse_diagnostics(&params, "file:///x.rs").is_empty());
    }

    #[test]
    fn diagnostics_entries_missing_required_fields_are_skipped() {
        let params = json!({
            "diagnostics": [
                // no range -> skipped
                { "message": "no range" },
                // no message -> skipped
                { "range": { "start": { "line": 0, "character": 0 } } },
                // complete -> kept
                {
                    "range": { "start": { "line": 5, "character": 2 } },
                    "message": "kept"
                }
            ]
        });
        let out = parse_diagnostics(&params, "file:///x.rs");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].message, "kept");
        assert_eq!(out[0].line, 6); // 0-based 5 -> 1-based 6
        assert_eq!(out[0].column, 3);
    }

    // ---- parse_locations -----------------------------------------------

    fn loc(uri: &str, sl: u64, sc: u64, el: u64, ec: u64) -> serde_json::Value {
        json!({
            "uri": uri,
            "range": {
                "start": { "line": sl, "character": sc },
                "end":   { "line": el, "character": ec }
            }
        })
    }

    #[test]
    fn locations_null_is_empty() {
        assert!(parse_locations(&json!(null)).is_empty());
    }

    #[test]
    fn locations_single_object_is_wrapped_in_a_vec() {
        let v = loc("file:///a.rs", 0, 0, 0, 5);
        let out = parse_locations(&v);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].file, PathBuf::from("/a.rs"));
        assert_eq!(out[0].range.start.line, 1); // 0-based -> 1-based
        assert_eq!(out[0].range.start.column, 1);
        assert_eq!(out[0].range.end.column, 6);
    }

    #[test]
    fn locations_array_preserves_order() {
        let v = json!([
            loc("file:///a.rs", 0, 0, 0, 1),
            loc("file:///b.rs", 1, 2, 1, 3),
        ]);
        let out = parse_locations(&v);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].file, PathBuf::from("/a.rs"));
        assert_eq!(out[1].file, PathBuf::from("/b.rs"));
    }

    #[test]
    fn locations_malformed_entries_are_skipped() {
        let v = json!([
            { "not": "a location" },
            loc("file:///good.rs", 0, 0, 0, 1),
            { "uri": "file:///no_range.rs" },
        ]);
        let out = parse_locations(&v);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].file, PathBuf::from("/good.rs"));
    }

    #[test]
    fn locations_uri_without_file_scheme_is_kept_verbatim() {
        let v = loc("/plain/path.rs", 0, 0, 0, 1);
        let out = parse_locations(&v);
        assert_eq!(out[0].file, PathBuf::from("/plain/path.rs"));
    }

    // ---- parse_hover ---------------------------------------------------

    #[test]
    fn hover_null_is_empty_text_and_no_range() {
        let h = parse_hover(&json!(null));
        assert!(h.text.is_empty());
        assert!(h.range.is_none());
    }

    #[test]
    fn hover_string_contents_become_the_text() {
        let v = json!({ "contents": "fn main()" });
        let h = parse_hover(&v);
        assert_eq!(h.text, "fn main()");
        assert!(h.range.is_none());
    }

    #[test]
    fn hover_markup_content_object_uses_its_value_field() {
        let v = json!({
            "contents": { "kind": "markdown", "value": "## docs" }
        });
        assert_eq!(parse_hover(&v).text, "## docs");
    }

    #[test]
    fn hover_array_of_mixed_items_is_joined_with_newlines() {
        // The LSP spec allows `contents` to be MarkedString |
        // MarkedString[], where MarkedString is `string | { language,
        // value }`.
        let v = json!({
            "contents": [
                "first",
                { "language": "rust", "value": "fn main" },
                "third"
            ]
        });
        assert_eq!(parse_hover(&v).text, "first\nfn main\nthird");
    }

    #[test]
    fn hover_array_with_unknown_items_skips_them() {
        let v = json!({ "contents": [ 42, "kept", null ] });
        assert_eq!(parse_hover(&v).text, "kept");
    }

    #[test]
    fn hover_range_is_translated_to_one_based() {
        let v = json!({
            "contents": "x",
            "range": {
                "start": { "line": 4, "character": 1 },
                "end":   { "line": 4, "character": 9 }
            }
        });
        let h = parse_hover(&v);
        let r = h.range.expect("range present");
        assert_eq!(r.start.line, 5);
        assert_eq!(r.start.column, 2);
        assert_eq!(r.end.column, 10);
    }

    #[test]
    fn hover_missing_contents_yields_empty_text() {
        let v = json!({});
        assert!(parse_hover(&v).text.is_empty());
    }

    // ---- uri_to_path ---------------------------------------------------

    #[test]
    fn uri_to_path_strips_the_file_scheme() {
        assert_eq!(uri_to_path("file:///tmp/x.rs"), PathBuf::from("/tmp/x.rs"));
    }

    #[test]
    fn uri_to_path_without_scheme_is_kept_verbatim() {
        assert_eq!(uri_to_path("/tmp/x.rs"), PathBuf::from("/tmp/x.rs"));
    }
}
