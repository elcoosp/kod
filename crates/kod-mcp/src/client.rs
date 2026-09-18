//! Newline-delimited JSON-RPC 2.0 client over a spawned server's stdio.
//!
//! # Framing
//!
//! The MCP stdio transport is line-delimited: one JSON object per
//! `\n`-terminated line, no embedded newlines. This client writes one
//! line per request and reads one line per incoming message. Any line
//! that fails to parse as JSON is logged and skipped — a server that
//! prints a banner on stdout is a spec violation but not a reason to
//! tear down the client for every subsequent call.
//!
//! # Concurrency
//!
//! `McpClient` is `Send + Sync`: the stdin handle is behind a mutex
//! (one write at a time), the pending-request map is behind a mutex
//! (one lookup/insert at a time), and the response channel is a
//! `tokio::sync::oneshot`. Multiple concurrent `call_tool` invocations
//! are safe; they serialize on the stdin write and run in parallel on
//! the response side.

use crate::types::{McpToolDef, McpToolResult, ServerInfo};
use std::collections::{BTreeMap, HashMap};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex, OnceCell, oneshot};

/// The per-request responder map. `i64` request ids map to a
/// oneshot that will carry the server's reply (or an error). The
/// alias exists because the raw type is a four-layer generic and
/// appears in three method signatures; naming it once keeps the
/// signatures readable and satisfies `clippy::type_complexity`.
type PendingMap = Arc<Mutex<HashMap<i64, oneshot::Sender<Result<serde_json::Value, McpError>>>>>;

/// `initialize` timeout. Generous: a Python MCP server may need to
/// import its modules on the first call, and a long-running one may
/// already be warm. 10 s covers both.
const INIT_TIMEOUT_SECS: u64 = 10;

/// `tools/list` and `tools/call` timeout. Configurable per call, but
/// the default covers a normal tool that reads a file or shells out.
const DEFAULT_CALL_TIMEOUT_SECS: u64 = 60;

#[derive(Debug, thiserror::Error)]
pub enum McpError {
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
    #[error("rpc error {code}: {message}")]
    Rpc { code: i64, message: String },
    #[error("timeout waiting for server response")]
    Timeout,
}

/// A running MCP server.
pub struct McpClient {
    child: Mutex<Child>,
    stdin: Mutex<ChildStdin>,
    next_id: AtomicI64,
    pending: PendingMap,
    server_info: OnceCell<ServerInfo>,
    program: String,
}

impl McpClient {
    /// Spawn a server and start the reader task. Does not perform the
    /// MCP handshake — call [`McpClient::initialize`] next.
    pub async fn spawn_stdio(
        cmd: &str,
        args: &[String],
        env: &BTreeMap<String, String>,
    ) -> Result<Self, McpError> {
        let mut command = Command::new(cmd);
        command
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // stderr is discarded: a server that logs to stderr on
            // every request would otherwise mix its diagnostics into
            // the terminal the TUI owns. A future revision can pipe it
            // to `tracing` when we need to debug a misbehaving server.
            .stderr(Stdio::null())
            .kill_on_drop(true);
        for (k, v) in env {
            command.env(k, v);
        }

        let mut child = command.spawn().map_err(|source| McpError::Spawn {
            program: cmd.to_string(),
            source,
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| McpError::Protocol("server has no stdin".to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| McpError::Protocol("server has no stdout".to_string()))?;

        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let pending_reader = pending.clone();

        tokio::spawn(async move {
            read_loop(stdout, pending_reader).await;
        });

        Ok(Self {
            child: Mutex::new(child),
            stdin: Mutex::new(stdin),
            next_id: AtomicI64::new(1),
            pending,
            server_info: OnceCell::new(),
            program: cmd.to_string(),
        })
    }

    /// The program this client spawned. Used by the adapter for tool
    /// naming and by error messages.
    pub fn program(&self) -> &str {
        &self.program
    }

    /// The server's self-reported info, once `initialize` succeeded.
    pub fn server_info(&self) -> Option<&ServerInfo> {
        self.server_info.get()
    }

    /// Perform the MCP handshake: send `initialize`, wait for the
    /// response, then send `notifications/initialized`. After this
    /// returns, the server is ready for `tools/list` and `tools/call`.
    pub async fn initialize(&self) -> Result<ServerInfo, McpError> {
        let params = serde_json::json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {
                "name": "kod",
                "version": env!("CARGO_PKG_VERSION"),
            }
        });
        let result = self
            .request("initialize", params, Duration::from_secs(INIT_TIMEOUT_SECS))
            .await?;
        let info = ServerInfo {
            name: result
                .get("serverInfo")
                .and_then(|s| s.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string(),
            version: result
                .get("serverInfo")
                .and_then(|s| s.get("version"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        };
        // The initialized notification has no response. A send failure
        // here means the server died between initialize and this call,
        // which the next request will surface. Best-effort.
        let _ = self
            .notify("notifications/initialized", serde_json::json!({}))
            .await;
        let _ = self.server_info.set(info.clone());
        Ok(info)
    }

    /// List the tools the server advertises.
    pub async fn list_tools(&self) -> Result<Vec<McpToolDef>, McpError> {
        let result = self
            .request(
                "tools/list",
                serde_json::json!({}),
                Duration::from_secs(DEFAULT_CALL_TIMEOUT_SECS),
            )
            .await?;
        let tools = result
            .get("tools")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let mut out = Vec::with_capacity(tools.len());
        for t in tools {
            match serde_json::from_value::<McpToolDef>(t) {
                Ok(tool) => out.push(tool),
                Err(e) => tracing::warn!(
                    program = %self.program,
                    error = %e,
                    "MCP: skipping malformed tool definition"
                ),
            }
        }
        Ok(out)
    }

    /// Invoke a tool by name. `args` is the JSON object the tool's
    /// `input_schema` describes; the server validates.
    pub async fn call_tool(
        &self,
        name: &str,
        args: serde_json::Value,
    ) -> Result<McpToolResult, McpError> {
        self.call_tool_with_timeout(name, args, DEFAULT_CALL_TIMEOUT_SECS)
            .await
    }

    /// Same as [`McpClient::call_tool`] with an explicit timeout, for a
    /// caller that knows a particular tool is slow.
    pub async fn call_tool_with_timeout(
        &self,
        name: &str,
        args: serde_json::Value,
        timeout_secs: u64,
    ) -> Result<McpToolResult, McpError> {
        let params = serde_json::json!({ "name": name, "arguments": args });
        let result = self
            .request("tools/call", params, Duration::from_secs(timeout_secs))
            .await?;
        let parsed: McpToolResult = serde_json::from_value(result)?;
        Ok(parsed)
    }

    /// Terminate the server. Best-effort: the child is killed and
    /// reaped, and the reader task exits when stdout closes.
    pub async fn shutdown(self) {
        let mut child = self.child.lock().await;
        let _ = child.start_kill();
        let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
    }

    // ------------------------------------------------------------------
    // Protocol plumbing
    // ------------------------------------------------------------------

    async fn request(
        &self,
        method: &str,
        params: serde_json::Value,
        timeout: Duration,
    ) -> Result<serde_json::Value, McpError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        if let Err(e) = self.send(&msg).await {
            self.pending.lock().await.remove(&id);
            return Err(e);
        }

        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err(McpError::Protocol(
                "response channel closed before a reply arrived".to_string(),
            )),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(McpError::Timeout)
            }
        }
    }

    async fn notify(&self, method: &str, params: serde_json::Value) -> Result<(), McpError> {
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        self.send(&msg).await
    }

    async fn send(&self, msg: &serde_json::Value) -> Result<(), McpError> {
        let mut line = serde_json::to_string(msg)?;
        line.push('\n');
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(line.as_bytes()).await?;
        stdin.flush().await?;
        Ok(())
    }
}

/// Background reader: parse one JSON object per line, dispatch by id.
async fn read_loop(stdout: ChildStdout, pending: PendingMap) {
    let mut reader = BufReader::new(stdout).lines();
    loop {
        match reader.next_line().await {
            Ok(Some(line)) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let msg: serde_json::Value = match serde_json::from_str(trimmed) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            line = %trimmed.chars().take(120).collect::<String>(),
                            "MCP: skipping unparseable line"
                        );
                        continue;
                    }
                };
                if let Some(id) = msg.get("id").and_then(|v| v.as_i64()) {
                    let sender = pending.lock().await.remove(&id);
                    if let Some(tx) = sender {
                        if let Some(err) = msg.get("error") {
                            let code = err.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
                            let message = err
                                .get("message")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let _ = tx.send(Err(McpError::Rpc { code, message }));
                        } else {
                            let result = msg
                                .get("result")
                                .cloned()
                                .unwrap_or(serde_json::Value::Null);
                            let _ = tx.send(Ok(result));
                        }
                    } else {
                        tracing::debug!(id, "MCP: response for an unknown or timed-out request");
                    }
                } else if let Some(method) = msg.get("method").and_then(|v| v.as_str()) {
                    // A notification (or a server-initiated request we
                    // do not implement). Logged and dropped — the MCP
                    // client is a client, not a server.
                    tracing::debug!(method, "MCP: server notification");
                }
            }
            Ok(None) => {
                tracing::debug!("MCP: server stdout closed");
                break;
            }
            Err(e) => {
                tracing::warn!(error = %e, "MCP: reader error");
                break;
            }
        }
    }
    // Reader exited (server died or closed stdout): fail every pending
    // request so callers do not hang until their timeouts.
    let mut guard = pending.lock().await;
    let drained: Vec<_> = guard.drain().collect();
    drop(guard);
    for (_, tx) in drained {
        let _ = tx.send(Err(McpError::Protocol("server terminated".to_string())));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// A tiny Python MCP server, written inline. It speaks just enough
    /// of the protocol for the three calls this client makes:
    /// `initialize`, `tools/list`, `tools/call`. Every response is a
    /// single line of JSON, matching the transport.
    ///
    /// Python is used rather than a shell script because the server
    /// must parse JSON to find the request id. A shell server would
    /// need `jq` and would be harder to read; the Python server is one
    /// heredoc and one dependency the CI image already has.
    const FAKE_SERVER: &str = r#"import json, sys
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    method = msg.get("method")
    msg_id = msg.get("id")
    if method == "initialize":
        out = {"jsonrpc":"2.0","id":msg_id,"result":{
            "protocolVersion":"2025-06-18",
            "capabilities":{"tools":{}},
            "serverInfo":{"name":"fake","version":"0.0.1"}
        }}
    elif method == "notifications/initialized":
        continue
    elif method == "tools/list":
        out = {"jsonrpc":"2.0","id":msg_id,"result":{"tools":[
            {"name":"echo","description":"echo back","inputSchema":{
                "type":"object","properties":{"text":{"type":"string"}}
            }},
            {"name":"fail","description":"always errors","inputSchema":{"type":"object"}}
        ]}}
    elif method == "tools/call":
        name = msg["params"]["name"]
        if name == "echo":
            text = msg["params"]["arguments"].get("text","")
            out = {"jsonrpc":"2.0","id":msg_id,"result":{
                "content":[{"type":"text","text":"echo: "+text}],
                "isError":False
            }}
        elif name == "fail":
            out = {"jsonrpc":"2.0","id":msg_id,"result":{
                "content":[{"type":"text","text":"deliberate failure"}],
                "isError":True
            }}
        else:
            out = {"jsonrpc":"2.0","id":msg_id,"error":{"code":-32601,"message":"no such tool"}}
    else:
        out = {"jsonrpc":"2.0","id":msg_id,"error":{"code":-32601,"message":"no such method"}}
    sys.stdout.write(json.dumps(out)+"\n")
    sys.stdout.flush()
"#;

    /// Write the fake server to a temp file and return its path.
    fn write_fake_server() -> tempfile::TempDir {
        let dir = tempfile::TempDir::new().expect("tempdir");
        std::fs::write(dir.path().join("server.py"), FAKE_SERVER).expect("write server");
        dir
    }

    fn python_available() -> bool {
        std::process::Command::new("python3")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    async fn spawn() -> (tempfile::TempDir, McpClient) {
        let dir = write_fake_server();
        let script = dir.path().join("server.py");
        let client = McpClient::spawn_stdio(
            "python3",
            &[script.to_string_lossy().to_string()],
            &BTreeMap::new(),
        )
        .await
        .expect("spawn fake server");
        (dir, client)
    }

    #[tokio::test]
    async fn initialize_returns_server_info() {
        if !python_available() {
            eprintln!("skipping: python3 not on PATH");
            return;
        }
        let (_dir, client) = spawn().await;
        let info = client.initialize().await.expect("initialize");
        assert_eq!(info.name, "fake");
        assert_eq!(info.version, "0.0.1");
        client.shutdown().await;
    }

    #[tokio::test]
    async fn list_tools_returns_both_tools() {
        if !python_available() {
            eprintln!("skipping: python3 not on PATH");
            return;
        }
        let (_dir, client) = spawn().await;
        client.initialize().await.expect("initialize");
        let tools = client.list_tools().await.expect("list");
        assert_eq!(tools.len(), 2);
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"echo"));
        assert!(names.contains(&"fail"));
        client.shutdown().await;
    }

    #[tokio::test]
    async fn call_tool_returns_rendered_text() {
        if !python_available() {
            eprintln!("skipping: python3 not on PATH");
            return;
        }
        let (_dir, client) = spawn().await;
        client.initialize().await.expect("initialize");
        let result = client
            .call_tool("echo", serde_json::json!({"text": "hello"}))
            .await
            .expect("call");
        assert!(!result.is_error);
        assert_eq!(result.render_text(), "echo: hello");
        client.shutdown().await;
    }

    #[tokio::test]
    async fn call_tool_reports_tool_level_error() {
        if !python_available() {
            eprintln!("skipping: python3 not on PATH");
            return;
        }
        let (_dir, client) = spawn().await;
        client.initialize().await.expect("initialize");
        let result = client
            .call_tool("fail", serde_json::json!({}))
            .await
            .expect("call");
        assert!(result.is_error, "isError should be true");
        assert!(result.render_text().contains("deliberate failure"));
        client.shutdown().await;
    }

    #[tokio::test]
    async fn rpc_error_propagates_for_unknown_tool() {
        if !python_available() {
            eprintln!("skipping: python3 not on PATH");
            return;
        }
        let (_dir, client) = spawn().await;
        client.initialize().await.expect("initialize");
        let err = client
            .call_tool("nope", serde_json::json!({}))
            .await
            .expect_err("unknown tool must error");
        match err {
            McpError::Rpc { code, message } => {
                assert_eq!(code, -32601);
                assert!(message.contains("no such tool"));
            }
            other => panic!("expected Rpc error, got {other:?}"),
        }
        client.shutdown().await;
    }

    #[tokio::test]
    async fn spawn_of_missing_binary_is_a_clear_error() {
        // `McpClient` does not implement `Debug` (its fields include
        // tokio handles whose `Debug` impls are internal), so
        // `expect_err` — which requires `Ok: Debug` — is unavailable.
        // Match on the result directly instead.
        let result =
            McpClient::spawn_stdio("this-binary-does-not-exist-kod-test", &[], &BTreeMap::new())
                .await;
        match result {
            Ok(_) => panic!("spawn of a missing binary must fail"),
            Err(McpError::Spawn { program, .. }) => {
                assert!(program.contains("this-binary-does-not-exist"));
            }
            Err(other) => panic!("expected Spawn error, got {other:?}"),
        }
    }
}
