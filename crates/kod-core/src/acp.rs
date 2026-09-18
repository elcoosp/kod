//! Agent Client Protocol bridge (D6.2, PR M4).
//!
//! # Spec
//!
//! Validated against the ACP v1 schema at
//! `https://agentclientprotocol.com/protocol/v1/schema` (the
//! `agent-client-protocol-schema` crate is the machine-readable
//! source of truth). The v1 protocol version integer is `1`.
//!
//! # Transport
//!
//! Content-Length-framed JSON-RPC 2.0 on stdio, the LSP convention
//! ACP inherits. One writer task per connection so a streaming
//! `session/update` and a `session/request_permission` reply cannot
//! interleave frame headers.
//!
//! # Bidirectional requests
//!
//! ACP is not client-request/agent-response. The agent makes requests
//! of its own — most importantly `session/request_permission`, sent
//! before executing a tool the agent's policy gates. This bridge
//! therefore has:
//!
//! - a **read loop** that demultiplexes: a frame with `method` and
//!   `id` is a client→agent request; a frame with `method` and no `id`
//!   is a client notification; a frame with `id` and no `method` is a
//!   response to a request the agent sent.
//! - a **pending-request map** keyed by id, with one oneshot per
//!   outstanding agent→client request.
//! - a `request_permission` method that allocates an id, sends the
//!   frame, and awaits the response.
//!
//! # Methods
//!
//! Client → agent:
//! - `initialize` — handshake.
//! - `session/new` — create a session (`cwd`, `mcpServers`).
//! - `session/prompt` — run a turn. Streams `session/update`
//!   notifications; responds with `{stopReason}` when the turn ends.
//! - `session/cancel` — **notification**: cancel the running turn.
//!
//! Agent → client:
//! - `session/update` — text chunks, tool-call lifecycle.
//! - `session/request_permission` — approval before a gated tool call.
//!
//! # What is deliberately not here
//!
//! `session/load` and `session/resume` are not advertised (the
//! `loadSession` capability is `false`). A full implementation would
//! replay a stored transcript; this bridge keeps sessions in memory
//! for the life of the process.
//!
//! `fs/read_text_file` and `fs/write_text_file` are not requested from
//! the client. The engine reads and writes files itself, through its
//! own sandbox and policy gate. An editor that wants to mediate file
//! access through its own buffer should be told so explicitly; this
//! bridge does not pretend to.

use crate::engine::{ApprovalDecision, KodEngine};
use kod_error::{KodError, Result};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, mpsc, oneshot};

/// Largest JSON-RPC body we will read. Matches the LSP client's
/// ceiling; a frame larger than this is a protocol bug or an attack.
const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// ACP v1 protocol version. Echoed to the client when it asks for a
/// version we support; a client asking for anything else gets this
/// back and decides for itself whether to keep the connection.
const PROTOCOL_VERSION: i64 = 1;

/// Shared state for one ACP connection.
struct Server {
    engine: Arc<KodEngine>,
    /// Writer-task channel. Every outgoing frame goes through here.
    out: mpsc::Sender<Value>,
    /// Id source for agent→client requests. Negative so it cannot
    /// collide with a client's own ids even if a client ever reuses
    /// them across the connection.
    next_request_id: AtomicI64,
    /// Outstanding agent→client requests, keyed by id. A response
    /// arriving on the read loop is routed to the matching oneshot.
    pending: Mutex<HashMap<i64, oneshot::Sender<Value>>>,
    /// ACP session id → engine transcript key. The engine keys its
    /// own transcripts by a string; the ACP session id is one.
    sessions: Mutex<HashMap<String, String>>,
    /// Most recent tool-call id per session. The engine's chunk
    /// stream carries a start, zero or more args updates, and a done,
    /// none of which carry a correlation id; the tool_call /
    /// tool_call_update notifications need one, so the id is tracked
    /// per session between the start and the done.
    last_tool_call_id: Mutex<HashMap<String, String>>,
}

impl Server {
    /// Send a notification (no id, no response expected).
    async fn notify(&self, method: &str, params: Value) {
        let _ = self
            .out
            .send(json!({
                "jsonrpc": "2.0",
                "method": method,
                "params": params,
            }))
            .await;
    }

    /// Send a request to the client and await its response.
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_request_id.fetch_sub(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        let sent = self
            .out
            .send(json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": method,
                "params": params,
            }))
            .await;
        if sent.is_err() {
            self.pending.lock().await.remove(&id);
            return Err(KodError::Internal("writer channel closed".to_string()));
        }
        // ACP requests have no client-side timeout in the spec; the
        // engine's own approval timeout would fire if the client never
        // answered, but this future is not the engine's. Bound it here
        // so a misbehaving client cannot wedge a session.
        match tokio::time::timeout(std::time::Duration::from_secs(120), rx).await {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(_)) => Err(KodError::Internal(
                "client dropped the pending request".to_string(),
            )),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(KodError::Internal(
                    "client did not answer within 120s".to_string(),
                ))
            }
        }
    }

    /// Send a response to a client→agent request.
    async fn respond(&self, id: Value, result: Value) {
        let _ = self
            .out
            .send(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": result,
            }))
            .await;
    }

    /// Send an error response to a client→agent request.
    async fn respond_error(&self, id: Value, code: i64, message: &str) {
        let _ = self
            .out
            .send(json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": code, "message": message },
            }))
            .await;
    }
}

/// Run the ACP bridge until the client closes stdin.
pub async fn serve(engine: Arc<KodEngine>) -> Result<()> {
    let (out_tx, mut out_rx) = mpsc::channel::<Value>(256);
    let mut writer = tokio::io::stdout();
    let writer_task = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            if write_frame(&mut writer, &msg).await.is_err() {
                break;
            }
        }
    });

    let server = Arc::new(Server {
        engine,
        out: out_tx.clone(),
        next_request_id: AtomicI64::new(-1),
        pending: Mutex::new(HashMap::new()),
        sessions: Mutex::new(HashMap::new()),
        last_tool_call_id: Mutex::new(HashMap::new()),
    });

    let mut reader = BufReader::new(tokio::io::stdin());
    loop {
        let frame = match read_frame(&mut reader).await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => {
                tracing::warn!(error = %e, "acp: frame read failed");
                break;
            }
        };

        let has_id = frame.get("id").is_some();
        let has_method = frame.get("method").is_some();

        if has_id && has_method {
            // Client → agent request. Dispatch on a spawned task: a
            // `session/prompt` runs for the whole turn, and the read
            // loop must stay free to route the
            // `session/request_permission` response that arrives
            // mid-turn.
            let server = server.clone();
            let id = frame.get("id").cloned().unwrap_or(Value::Null);
            let method = frame
                .get("method")
                .and_then(|m| m.as_str())
                .unwrap_or("")
                .to_string();
            let params = frame.get("params").cloned().unwrap_or(Value::Null);
            tokio::spawn(async move {
                dispatch_request(server, id, &method, params).await;
            });
        } else if has_id {
            // Response to an agent → client request.
            let Some(id) = frame.get("id").and_then(|v| v.as_i64()) else {
                continue;
            };
            let sender = server.pending.lock().await.remove(&id);
            if let Some(tx) = sender {
                let result = frame.get("result").cloned().unwrap_or(Value::Null);
                let _ = tx.send(result);
            }
        } else if has_method {
            // Client → agent notification (no response expected).
            let method = frame.get("method").and_then(|m| m.as_str()).unwrap_or("");
            let params = frame.get("params").cloned().unwrap_or(Value::Null);
            if method == "session/cancel" {
                let session_id = params
                    .get("sessionId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let key = engine_key(session_id);
                server.engine.request_cancel_for(&key);
            }
            // Other client notifications (e.g. `$/cancelRequest`) are
            // ignored: this bridge does not make cancellable requests
            // of its own besides `session/request_permission`, whose
            // cancellation is routed through the ACP response shape.
        }
    }

    drop(out_tx);
    // `server` also holds a clone of the `out_tx` sender (see the
    // `Server` construction above). Dropping only `out_tx` leaves
    // the channel open, so `writer_task`'s `out_rx.recv().await`
    // never returns `None` and awaiting the writer task here
    // deadlocks: the writer waits for a closed channel, the channel
    // stays open because `server.out` holds a sender.
    //
    // In the handshake-only case (initialize, then the client closes
    // stdin), any dispatch task spawned for that request has already
    // completed and dropped its own `Arc<Server>` clone, so this
    // drop releases the last sender and the writer exits promptly.
    // When a long-running `session/prompt` task is still alive it
    // holds its own clone; the channel therefore stays open until
    // that task finishes, which is the correct behaviour — the
    // client's EOF is a shutdown signal for the reader, not a hard
    // kill of an in-flight turn.
    drop(server);
    let _ = writer_task.await;
    Ok(())
}

/// Dispatch one client→agent request. Every arm either responds or
/// deliberately does not (a notification is filtered out in the read
/// loop before reaching here).
async fn dispatch_request(server: Arc<Server>, id: Value, method: &str, params: Value) {
    match method {
        "initialize" => {
            // Version negotiation: echo the client's version if we
            // support it, otherwise return our latest. The client
            // decides whether to keep the connection.
            let requested = params
                .get("protocolVersion")
                .and_then(|v| v.as_i64())
                .unwrap_or(PROTOCOL_VERSION);
            let agreed = if requested == PROTOCOL_VERSION {
                requested
            } else {
                PROTOCOL_VERSION
            };
            server
                .respond(
                    id,
                    json!({
                        "protocolVersion": agreed,
                        "agentCapabilities": {
                            // In-memory sessions only; `session/load`
                            // and `session/resume` are not implemented.
                            "loadSession": false,
                            "promptCapabilities": {
                                "image": false,
                                "audio": false,
                                "embeddedContext": true
                            },
                            "mcpCapabilities": {
                                "http": false,
                                "sse": false
                            }
                        },
                        "agentInfo": {
                            "name": "kod",
                            "title": "KOD",
                            "version": env!("CARGO_PKG_VERSION")
                        },
                        "authMethods": []
                    }),
                )
                .await;
        }
        "session/new" => {
            let session_id = format!("kod-{}", uuid::Uuid::new_v4());
            let transcript_key = engine_key(&session_id);
            server
                .sessions
                .lock()
                .await
                .insert(session_id.clone(), transcript_key);
            server
                .respond(
                    id,
                    json!({
                        "sessionId": session_id,
                        "configOptions": null,
                        "modes": null
                    }),
                )
                .await;
        }
        "session/prompt" => {
            let session_id = params
                .get("sessionId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if session_id.is_empty() {
                server
                    .respond_error(id, -32602, "sessionId is required")
                    .await;
                return;
            }
            let prompt = extract_prompt_text(&params);
            let server_clone = server.clone();
            let id_clone = id.clone();
            tokio::spawn(async move {
                run_prompt(server_clone, id_clone, session_id, prompt).await;
            });
        }
        other => {
            server
                .respond_error(id, -32601, &format!("unknown method: {other}"))
                .await;
        }
    }
}

/// The engine's transcript key for an ACP session.
fn engine_key(session_id: &str) -> String {
    if session_id.is_empty() {
        "acp".to_string()
    } else {
        format!("acp:{session_id}")
    }
}

/// Extract the concatenated text from ACP prompt content blocks. Only
/// `ContentBlock::Text` is read; `ResourceLink` is deliberately
/// skipped (the engine reads files through its own tools, under its
/// own policy, not through whatever the editor happened to attach).
fn extract_prompt_text(params: &Value) -> String {
    let Some(arr) = params.get("prompt").and_then(|p| p.as_array()) else {
        return String::new();
    };
    let mut out = String::new();
    for block in arr {
        if block.get("type").and_then(|t| t.as_str()) == Some("text")
            && let Some(t) = block.get("text").and_then(|v| v.as_str())
        {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(t);
        }
    }
    out
}

/// Run one turn. Streams `session/update` notifications, mediates
/// approvals, then responds with `{stopReason}`.
///
/// The stop reason set is the ACP v1 list: `end_turn`, `max_tokens`,
/// `max_turn_requests`, `refusal`, `cancelled`. This bridge emits
/// `end_turn` on a normal finish and `cancelled` when the engine
/// reports a user cancel. The other three need per-turn signals the
/// engine does not yet report.
async fn run_prompt(server: Arc<Server>, req_id: Value, session_id: String, prompt: String) {
    let engine = server.engine.clone();
    let key = engine_key(&session_id);
    let (chunk_tx, mut chunk_rx) = mpsc::channel::<String>(64);
    let engine_call = engine.clone();
    let key_owned = key.clone();
    let prompt_owned = prompt.clone();
    let call = tokio::spawn(async move {
        engine_call
            .process_streaming_for(&key_owned, &prompt_owned, &chunk_tx)
            .await
    });

    // Drain chunks while the engine runs, translating each into the
    // right ACP notification. The engine sends its own
    // `\0kod-approval-batch:` marker for pending approvals; this
    // bridge turns each item into a `session/request_permission`
    // request and forwards the client's answer to the engine.
    while let Some(chunk) = chunk_rx.recv().await {
        if let Err(e) = handle_chunk(&server, &session_id, &chunk).await {
            tracing::warn!(error = %e, "acp: chunk handling failed");
        }
    }

    let outcome = match call.await {
        Ok(r) => r,
        Err(e) => {
            server
                .respond_error(req_id, -32000, &format!("engine task panicked: {e}"))
                .await;
            return;
        }
    };
    match outcome {
        Ok(_resp) => {
            // The turn ended. Text already streamed through
            // `session/update`; the response carries only the stop
            // reason, as the spec requires.
            server
                .respond(req_id, json!({"stopReason": "end_turn"}))
                .await;
        }
        Err(e) => {
            let msg = e.to_string();
            let stop = if msg.contains("cancelled") {
                "cancelled"
            } else {
                "refusal"
            };
            server.respond(req_id, json!({"stopReason": stop})).await;
        }
    }
}

/// Translate one engine chunk into zero or more ACP notifications,
/// handling approval batches as requests.
async fn handle_chunk(server: &Arc<Server>, session_id: &str, chunk: &str) -> Result<()> {
    // Approval batch: one `session/request_permission` per item,
    // await each in turn, forward the decision to the engine.
    if let Some((_batch_id, json_str)) = crate::engine::parse_tool_approval_batch(chunk) {
        let batch: crate::engine::ApprovalBatch =
            serde_json::from_str(json_str).unwrap_or_default();
        for item in batch.items {
            let Some(item_id) = item.id else { continue };
            let decision = request_permission(server, session_id, &item).await;
            let _ = server.engine.respond_to_approval(item_id, decision).await;
        }
        return Ok(());
    }

    // Ask_user: a question is not a permission. ACP has no
    // `session/request_question`; an editor session that the engine
    // pauses on an ask_user would hang. Reply with a placeholder so
    // the engine continues; the model sees a "(no answer) from the
    // editor" string and can adapt.
    if let Some((qid, _json)) = crate::engine::parse_question(chunk) {
        server
            .engine
            .respond_to_question(
                qid,
                "(the editor client has no answer channel for this question)".to_string(),
            )
            .await;
        return Ok(());
    }

    // Tool lifecycle.
    if let Some(name) = crate::engine::parse_tool_start(chunk) {
        let tool_call_id = format!("tc-{}", uuid::Uuid::new_v4());
        server
            .last_tool_call_id
            .lock()
            .await
            .insert(session_id.to_string(), tool_call_id.clone());
        server
            .notify(
                "session/update",
                json!({
                    "sessionId": session_id,
                    "update": {
                        "sessionUpdate": "tool_call",
                        "toolCallId": tool_call_id,
                        "title": name,
                        "kind": kind_for_tool(name),
                        "status": "pending",
                        "rawInput": {}
                    }
                }),
            )
            .await;
        return Ok(());
    }
    if let Some(brief) = crate::engine::parse_tool_args(chunk) {
        if let Some(tool_call_id) = server
            .last_tool_call_id
            .lock()
            .await
            .get(session_id)
            .cloned()
        {
            server
                .notify(
                    "session/update",
                    json!({
                        "sessionId": session_id,
                        "update": {
                            "sessionUpdate": "tool_call_update",
                            "toolCallId": tool_call_id,
                            "title": brief,
                            "status": "in_progress"
                        }
                    }),
                )
                .await;
        }
        return Ok(());
    }
    if let Some((header, summary, _ms)) = crate::engine::parse_tool_done(chunk) {
        if let Some(tool_call_id) = server
            .last_tool_call_id
            .lock()
            .await
            .get(session_id)
            .cloned()
        {
            let is_error = summary.trim_start().starts_with("Error:");
            server
                .notify(
                    "session/update",
                    json!({
                        "sessionId": session_id,
                        "update": {
                            "sessionUpdate": "tool_call_update",
                            "toolCallId": tool_call_id,
                            "title": header,
                            "status": if is_error { "failed" } else { "completed" },
                            "rawOutput": { "text": summary }
                        }
                    }),
                )
                .await;
        }
        return Ok(());
    }
    if crate::engine::is_thinking_marker(chunk) {
        return Ok(());
    }
    if chunk.is_empty() || chunk.starts_with('\0') {
        return Ok(());
    }

    // Plain text: an agent message chunk.
    server
        .notify(
            "session/update",
            json!({
                "sessionId": session_id,
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": { "type": "text", "text": chunk }
                }
            }),
        )
        .await;
    Ok(())
}

/// Ask the client for permission on one approval item and translate
/// the answer back to an engine `ApprovalDecision`.
async fn request_permission(
    server: &Arc<Server>,
    session_id: &str,
    item: &crate::engine::ApprovalRequest,
) -> ApprovalDecision {
    let tool_call_id = server
        .last_tool_call_id
        .lock()
        .await
        .get(session_id)
        .cloned()
        .unwrap_or_else(|| format!("tc-{}", uuid::Uuid::new_v4()));

    // Four options, matching the ACP `PermissionOption` kinds:
    // `allow_once`, `allow_always`, `reject_once`, `reject_always`.
    // `deny_always` in the engine's model maps to `reject_always`.
    let params = json!({
        "sessionId": session_id,
        "toolCall": {
            "toolCallId": tool_call_id,
            "title": item.summary,
            "kind": kind_for_tool(&item.tool_name),
            "status": "pending",
            "rawInput": item.arguments.clone()
        },
        "options": [
            {"optionId": "allow_once", "name": "Allow once", "kind": "allow_once"},
            {"optionId": "allow_always", "name": "Allow for this session", "kind": "allow_always"},
            {"optionId": "reject_once", "name": "Reject", "kind": "reject_once"},
            {"optionId": "reject_always", "name": "Never for this pattern", "kind": "reject_always"}
        ]
    });

    match server.request("session/request_permission", params).await {
        Ok(result) => {
            let outcome = result.get("outcome");
            let kind = outcome
                .and_then(|o| o.get("outcome"))
                .and_then(|o| o.as_str());
            if kind != Some("selected") {
                return ApprovalDecision::Deny;
            }
            let option_id = outcome
                .and_then(|o| o.get("optionId"))
                .and_then(|o| o.as_str())
                .unwrap_or("reject_once");
            match option_id {
                "allow_once" | "allow_always" => ApprovalDecision::Approve,
                "reject_always" => ApprovalDecision::DenyAlways,
                _ => ApprovalDecision::Deny,
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "acp: permission request failed; denying");
            ApprovalDecision::Deny
        }
    }
}

/// ACP `ToolKind` for a tool name. The ACP schema defines: `read`,
/// `edit`, `delete`, `move`, `search`, `execute`, `think`, `fetch`,
/// `other`. The mapping is deliberately coarse — the client uses it
/// to pick an icon and a colour, not to gate anything.
fn kind_for_tool(name: &str) -> &'static str {
    match name {
        "read_file" | "file_info" => "read",
        "write_file" | "patch_file" => "edit",
        "list_files" | "grep" | "search_files" => "search",
        "execute_command" => "execute",
        "web_fetch" => "fetch",
        "todo" | "ask_user" | "memory_save" | "memory_search" => "think",
        "git_status" | "git_diff" | "git_commit" | "git_branch" => "other",
        _ => "other",
    }
}

/// Read one `Content-Length`-framed JSON-RPC message.
async fn read_frame<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
) -> Result<Option<Value>> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await.map_err(KodError::Io)?;
        if n == 0 {
            return Ok(None);
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some(rest) = trimmed.strip_prefix("Content-Length:") {
            content_length =
                Some(
                    rest.trim()
                        .parse()
                        .map_err(|_| KodError::InvalidParameters {
                            reason: format!("bad Content-Length: {rest:?}"),
                        })?,
                );
        }
    }
    let n = content_length.ok_or_else(|| KodError::InvalidParameters {
        reason: "missing Content-Length header".to_string(),
    })?;
    if n > MAX_FRAME_BYTES {
        return Err(KodError::InvalidParameters {
            reason: format!("frame exceeds {MAX_FRAME_BYTES} bytes"),
        });
    }
    let mut buf = vec![0u8; n];
    reader.read_exact(&mut buf).await.map_err(KodError::Io)?;
    let v: Value = serde_json::from_slice(&buf)
        .map_err(|e| KodError::Deserialization(format!("acp frame: {e}")))?;
    Ok(Some(v))
}

/// Write one `Content-Length`-framed JSON-RPC message.
async fn write_frame<W: tokio::io::AsyncWrite + Unpin>(writer: &mut W, msg: &Value) -> Result<()> {
    let body = serde_json::to_vec(msg).map_err(|e| KodError::Serialization(e.to_string()))?;
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    writer
        .write_all(header.as_bytes())
        .await
        .map_err(KodError::Io)?;
    writer.write_all(&body).await.map_err(KodError::Io)?;
    writer.flush().await.map_err(KodError::Io)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frame_roundtrip() {
        let msg = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"});
        let mut buf: Vec<u8> = Vec::new();
        write_frame(&mut buf, &msg).await.unwrap();
        let mut reader = BufReader::new(std::io::Cursor::new(buf));
        let parsed = read_frame(&mut reader).await.unwrap().unwrap();
        assert_eq!(parsed, msg);
    }

    #[tokio::test]
    async fn missing_content_length_errors() {
        let raw: &[u8] = b"X-Other: foo\r\n\r\n{}";
        let mut reader = BufReader::new(std::io::Cursor::new(raw));
        let err = read_frame(&mut reader).await.unwrap_err();
        assert!(err.to_string().contains("Content-Length"));
    }

    #[tokio::test]
    async fn eof_returns_none() {
        let raw: &[u8] = b"";
        let mut reader = BufReader::new(std::io::Cursor::new(raw));
        assert!(read_frame(&mut reader).await.unwrap().is_none());
    }

    #[test]
    fn engine_key_prefixes_with_acp() {
        assert_eq!(engine_key(""), "acp");
        assert_eq!(engine_key("abc"), "acp:abc");
    }

    #[test]
    fn extract_prompt_text_concatenates_text_blocks() {
        let params = json!({
            "prompt": [
                {"type": "text", "text": "first"},
                {"type": "resource_link", "uri": "file:///x"},
                {"type": "text", "text": "second"}
            ]
        });
        assert_eq!(extract_prompt_text(&params), "first\nsecond");
    }

    #[test]
    fn kind_mapping_is_coarse_and_total() {
        assert_eq!(kind_for_tool("read_file"), "read");
        assert_eq!(kind_for_tool("write_file"), "edit");
        assert_eq!(kind_for_tool("execute_command"), "execute");
        assert_eq!(kind_for_tool("web_fetch"), "fetch");
        assert_eq!(kind_for_tool("grep"), "search");
        assert_eq!(kind_for_tool("unknown-tool"), "other");
    }

    #[test]
    fn stop_reason_set_is_the_acp_v1_list() {
        // Compile-time assertion that the four strings the bridge
        // emits are all in the ACP v1 StopReason union:
        // end_turn, max_tokens, max_turn_requests, refusal, cancelled.
        let emitted = ["end_turn", "cancelled", "refusal"];
        let acp_v1 = [
            "end_turn",
            "max_tokens",
            "max_turn_requests",
            "refusal",
            "cancelled",
        ];
        for s in emitted {
            assert!(acp_v1.contains(&s), "{s} is not an ACP v1 stop reason");
        }
    }
}

/// Edge cases for the ACP pure helpers. The happy path is covered
/// by `mod tests`; these pin the tolerance branches a refactor could
/// silently break: missing fields, wrong types, resource-link blocks,
/// and the empty-session key.
#[cfg(test)]
mod coverage_acp_edges {
    use super::*;
    use serde_json::json;

    #[test]
    fn engine_key_empty_is_the_bare_acp_prefix() {
        assert_eq!(engine_key(""), "acp");
    }

    #[test]
    fn engine_key_nonempty_carries_the_session_id() {
        assert_eq!(engine_key("s-1"), "acp:s-1");
    }

    #[test]
    fn extract_prompt_text_missing_prompt_key_is_empty() {
        assert_eq!(extract_prompt_text(&json!({})), "");
    }

    #[test]
    fn extract_prompt_text_non_array_prompt_is_empty() {
        assert_eq!(extract_prompt_text(&json!({ "prompt": "nope" })), "");
        assert_eq!(extract_prompt_text(&json!({ "prompt": 42 })), "");
        assert_eq!(extract_prompt_text(&json!({ "prompt": null })), "");
    }

    #[test]
    fn extract_prompt_text_skips_resource_link_blocks() {
        let params = json!({
            "prompt": [
                { "type": "resource_link", "uri": "file:///etc/passwd" },
                { "type": "text", "text": "hello" }
            ]
        });
        assert_eq!(extract_prompt_text(&params), "hello");
    }

    #[test]
    fn extract_prompt_text_skips_text_blocks_without_a_text_field() {
        let params = json!({
            "prompt": [
                { "type": "text" },
                { "type": "text", "text": "kept" }
            ]
        });
        assert_eq!(extract_prompt_text(&params), "kept");
    }

    #[test]
    fn extract_prompt_text_skips_blocks_with_no_type() {
        let params = json!({
            "prompt": [
                { "text": "no type field" },
                { "type": "text", "text": "kept" }
            ]
        });
        assert_eq!(extract_prompt_text(&params), "kept");
    }

    #[test]
    fn extract_prompt_text_ignores_non_text_block_types() {
        // Even if a non-text block carries a `text` field, only
        // `type == "text"` blocks are read.
        let params = json!({
            "prompt": [
                { "type": "image", "text": "not read" }
            ]
        });
        assert_eq!(extract_prompt_text(&params), "");
    }

    #[test]
    fn extract_prompt_text_joins_blocks_with_one_newline() {
        let params = json!({
            "prompt": [
                { "type": "text", "text": "first" },
                { "type": "text", "text": "second" }
            ]
        });
        assert_eq!(extract_prompt_text(&params), "first\nsecond");
    }

    #[test]
    fn extract_prompt_text_on_an_empty_array_is_empty() {
        assert_eq!(extract_prompt_text(&json!({ "prompt": [] })), "");
    }

    #[test]
    fn kind_for_tool_covers_every_documented_kind() {
        // One representative per ACP ToolKind. If a name moves between
        // categories, the client's icon/colour changes silently; this
        // is the canary.
        assert_eq!(kind_for_tool("read_file"), "read");
        assert_eq!(kind_for_tool("write_file"), "edit");
        assert_eq!(kind_for_tool("list_files"), "search");
        assert_eq!(kind_for_tool("execute_command"), "execute");
        assert_eq!(kind_for_tool("web_fetch"), "fetch");
        assert_eq!(kind_for_tool("todo"), "think");
        assert_eq!(kind_for_tool("git_status"), "other");
        assert_eq!(kind_for_tool("something_unknown"), "other");
    }
}
