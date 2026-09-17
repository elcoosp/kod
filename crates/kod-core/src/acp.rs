//! Agent Client Protocol bridge (D6.2, PR M4).
//!
//! # Status
//!
//! Best-effort. ACP is a young spec; the method names, capability
//! keys, and the shape of `session/update` notifications written
//! here match the ACP design at the time of writing but have not
//! been validated against a live Zed client. The transport
//! (Content-Length-framed JSON-RPC 2.0 over stdio, the LSP
//! convention ACP inherits) is the stable part.
//!
//! Before publishing `kod acp` in a release, verify against:
//!   https://github.com/zed-industries/agent-client-protocol
//!
//! # Methods
//!
//! Client to server:
//! - `initialize` — handshake.
//! - `session/new` — create a session.
//! - `session/prompt` — send a turn; streams `session/update`
//!   notifications while it runs, then responds with `stopReason`.
//! - `session/cancel` — cancel the running turn.
//!
//! Server to client:
//! - `session/update` — text chunks and tool-call notices.
//!
//! # What is deliberately not here
//!
//! `session/request_permission`: approvals are dropped on the floor.
//! An editor-mediated permission round trip needs a request-id
//! allocator, a pending map, and a read loop that demultiplexes
//! responses from requests — the exact shape `kod-mcp`'s client has
//! in reverse. This bridge works with a permissive policy on the
//! engine side; a follow-up adds the round trip.
//!
//! `kod mcp --server` (mode serveur MCP stdio): not implemented.
//! The MCP *client* is at `crates/kod-mcp`.
//!
//! # Relationship to `kod serve`
//!
//! Both wrap `KodEngine::process_streaming_for`. `kod serve` speaks
//! NDJSON on a Unix socket for a terminal client; `kod acp` speaks
//! Content-Length-framed JSON-RPC on stdio for an editor that
//! spawned the process.

use crate::engine::KodEngine;
use kod_error::{KodError, Result};
use serde_json::Value;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

/// Largest JSON-RPC body we will read. Matches the LSP client's
/// ceiling; a frame larger than this is a protocol bug or an attack.
const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// Run the ACP bridge until the client closes stdin.
pub async fn serve(engine: Arc<KodEngine>) -> Result<()> {
    let mut reader = BufReader::new(tokio::io::stdin());
    let mut writer = tokio::io::stdout();

    // One writer task, one channel. Two concurrent writers framing
    // JSON-RPC on the same stdout would interleave `Content-Length`
    // headers with bodies; the channel makes the framing
    // single-threaded without locking.
    let (out_tx, mut out_rx) = mpsc::channel::<Value>(256);
    let writer_task = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            if write_frame(&mut writer, &msg).await.is_err() {
                break;
            }
        }
    });

    loop {
        let msg = match read_frame(&mut reader).await {
            Ok(Some(m)) => m,
            Ok(None) => break,
            Err(e) => {
                tracing::warn!(error = %e, "acp: frame read failed");
                break;
            }
        };
        let method = msg
            .get("method")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_string();
        let id = msg.get("id").cloned();
        let params = msg.get("params").cloned().unwrap_or(Value::Null);

        match method.as_str() {
            "initialize" => {
                let result = serde_json::json!({
                    "protocolVersion": 1,
                    "agentCapabilities": {
                        "loadSession": false,
                        "promptCapabilities": {
                            "image": false,
                            "audio": false,
                            "embeddedContext": false
                        }
                    }
                });
                respond(&out_tx, id, result).await?;
            }
            "session/new" => {
                let session_id = uuid::Uuid::new_v4().to_string();
                respond(
                    &out_tx,
                    id,
                    serde_json::json!({ "sessionId": session_id }),
                )
                .await?;
            }
            "session/prompt" => {
                let engine_task = engine.clone();
                let out = out_tx.clone();
                let session_id = params
                    .get("sessionId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let text = extract_prompt_text(&params);
                let req_id = id.clone();
                tokio::spawn(async move {
                    if let Err(e) =
                        run_prompt(&engine_task, &out, &session_id, &text, req_id)
                            .await
                    {
                        tracing::warn!(error = %e, "acp: prompt task failed");
                    }
                });
            }
            "session/cancel" => {
                let session_id = params
                    .get("sessionId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                engine.request_cancel_for(&transcript_key(session_id));
                respond(&out_tx, id, serde_json::json!({})).await?;
            }
            other if !other.is_empty() => {
                respond_error(
                    &out_tx,
                    id,
                    -32601,
                    &format!("unknown method: {other}"),
                )
                .await?;
            }
            _ => {
                // A response to a request we would have sent; this
                // revision does not send any (see module doc).
            }
        }
    }

    drop(out_tx);
    let _ = writer_task.await;
    Ok(())
}

/// Run one turn, streaming `session/update` notifications, then
/// respond to the `session/prompt` request with the stop reason.
async fn run_prompt(
    engine: &Arc<KodEngine>,
    out: &mpsc::Sender<Value>,
    session_id: &str,
    prompt: &str,
    req_id: Option<Value>,
) -> Result<()> {
    let key = transcript_key(session_id);
    let (chunk_tx, mut chunk_rx) = mpsc::channel::<String>(64);
    let engine_call = engine.clone();
    let key_owned = key.clone();
    let prompt_owned = prompt.to_string();
    let call = tokio::spawn(async move {
        engine_call
            .process_streaming_for(&key_owned, &prompt_owned, &chunk_tx)
            .await
    });
    while let Some(chunk) = chunk_rx.recv().await {
        if let Some(notification) = chunk_to_update(session_id, &chunk) {
            let _ = out.send(notification).await;
        }
    }
    let outcome = call
        .await
        .map_err(|e| KodError::Internal(format!("engine task panicked: {e}")))?;
    match outcome {
        Ok(resp) => {
            if let Some(id) = req_id {
                let text = resp.text.unwrap_or_default();
                send_response(
                    out,
                    id,
                    serde_json::json!({
                        "stopReason": "end_turn",
                        "text": text,
                    }),
                )
                .await;
            }
        }
        Err(e) => {
            if let Some(id) = req_id {
                send_error(out, id, -32000, &e.to_string()).await;
            }
        }
    }
    Ok(())
}

/// The engine transcript key for a session. Empty ACP ids (should
/// not happen, but a malformed client could send one) map to a
/// shared `acp` key rather than the interactive session's `""`.
fn transcript_key(session_id: &str) -> String {
    if session_id.is_empty() {
        "acp".to_string()
    } else {
        format!("acp:{session_id}")
    }
}

/// Extract the concatenated text from ACP's `prompt` content blocks.
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

/// Map one engine chunk to an ACP `session/update` notification.
/// The tool-args marker becomes a tool-call notice; plain text
/// becomes an agent message chunk; other control markers are
/// dropped (ACP has no place for them).
fn chunk_to_update(session_id: &str, chunk: &str) -> Option<Value> {
    if let Some(brief) = crate::engine::parse_tool_args(chunk) {
        return Some(serde_json::json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": session_id,
                "update": {
                    "sessionUpdate": "tool_call",
                    "title": brief
                }
            }
        }));
    }
    if chunk.starts_with('\0') || chunk.is_empty() {
        return None;
    }
    Some(serde_json::json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "sessionId": session_id,
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "content": { "type": "text", "text": chunk }
            }
        }
    }))
}

async fn respond(
    out: &mpsc::Sender<Value>,
    id: Option<Value>,
    result: Value,
) -> Result<()> {
    if let Some(id) = id {
        send_response(out, id, result).await;
    }
    Ok(())
}

async fn send_response(out: &mpsc::Sender<Value>, id: Value, result: Value) {
    let _ = out
        .send(serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        }))
        .await;
}

async fn send_error(out: &mpsc::Sender<Value>, id: Value, code: i64, message: &str) {
    let _ = out
        .send(serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message },
        }))
        .await;
}

async fn respond_error(
    out: &mpsc::Sender<Value>,
    id: Option<Value>,
    code: i64,
    message: &str,
) -> Result<()> {
    if let Some(id) = id {
        send_error(out, id, code, message).await;
    }
    Ok(())
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
                Some(rest.trim().parse().map_err(|_| KodError::InvalidParameters {
                    reason: format!("bad Content-Length: {rest:?}"),
                })?);
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
async fn write_frame<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    msg: &Value,
) -> Result<()> {
    let body =
        serde_json::to_vec(msg).map_err(|e| KodError::Serialization(e.to_string()))?;
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
        let msg = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"});
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
    fn transcript_key_prefixes_with_acp() {
        assert_eq!(transcript_key(""), "acp");
        assert_eq!(transcript_key("abc"), "acp:abc");
    }

    #[test]
    fn extract_prompt_text_concatenates_text_blocks() {
        let params = serde_json::json!({
            "prompt": [
                {"type": "text", "text": "first"},
                {"type": "image", "data": "..."},
                {"type": "text", "text": "second"}
            ]
        });
        assert_eq!(extract_prompt_text(&params), "first\nsecond");
    }

    #[test]
    fn chunk_to_update_drops_control_markers() {
        assert!(chunk_to_update("s", "\u{0}kod-thinking\u{0}").is_none());
        assert!(chunk_to_update("s", "").is_none());
        let v = chunk_to_update("s", "hello").unwrap();
        assert_eq!(v["method"], "session/update");
        assert_eq!(v["params"]["update"]["sessionUpdate"], "agent_message_chunk");
    }

    #[test]
    fn chunk_to_update_maps_tool_args_to_tool_call() {
        let chunk = crate::engine::tool_args_marker("read_file path=x");
        let v = chunk_to_update("s", &chunk).unwrap();
        assert_eq!(v["params"]["update"]["sessionUpdate"], "tool_call");
        assert_eq!(v["params"]["update"]["title"], "read_file path=x");
    }
}
