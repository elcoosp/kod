//! ACP `session/request_permission` approval round-trip (session 16,
//! behavioral-integration arc).
//!
//! `acp_session.rs` drives the plain turn: prompt -> streamed text ->
//! `{stopReason: end_turn}`. This file drives the other path the
//! bridge's `run_prompt` owns: a gated tool call triggers a
//! `session/request_permission` request mid-stream, the client answers
//! it, and the engine either runs or skips the call depending on the
//! answer. Nothing outside `run_prompt` exercised that branch before
//! this file.
//!
//! Two tests:
//!   1. approve  — client answers `allow_once`; the file is written;
//!                 the prompt completes with `end_turn`.
//!   2. deny     — client answers `reject_once`; the file is NOT
//!                 written; the tool result is an error; the prompt
//!                 still completes (a denied call is not a failed
//!                 turn).
//!
//! The setup mirrors acp_session.rs: `serve_io` over a duplex pair, a
//! `MockProvider`-backed engine, no subprocess or network.

use kod_core::KodEngine;
use kod_core::router::RouterConfig;
use kod_config::{Decision, Policy, PolicyEngine, Preset, ToolPolicy};
use kod_provider::testkit::{MockProvider, Script};
use kod_types::ToolCall;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

/// Write one `Content-Length`-framed ACP message (the wire the bridge
/// speaks on the server side of the duplex).
async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, msg: &Value) -> std::io::Result<()> {
    let body = serde_json::to_vec(msg).unwrap();
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    w.write_all(header.as_bytes()).await?;
    w.write_all(&body).await?;
    w.flush().await?;
    Ok(())
}

/// Read one `Content-Length`-framed ACP message. `Ok(None)` on clean
/// EOF (server side closed its writer when the reader finished).
async fn read_frame<R: AsyncRead + Unpin>(
    r: &mut BufReader<R>,
) -> std::io::Result<Option<Value>> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        let n = r.read_line(&mut line).await?;
        if n == 0 {
            return Ok(None);
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some(rest) = trimmed.strip_prefix("Content-Length: ") {
            content_length = rest.parse::<usize>().ok();
        }
    }
    let len = content_length.expect("frame with no Content-Length");
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await?;
    Ok(Some(serde_json::from_slice(&buf).expect("frame body is JSON")))
}

/// What `read_until_response` saw while waiting for the prompt's
/// response. Split out so a test can assert both "notifications
/// streamed" and "an approval request was sent" from the same read.
struct ReadOutcome {
    /// The final response frame for the request we were waiting for.
    response: Value,
    /// Notifications (frames with `method` and no `id`) received before
    /// the response, in arrival order. Includes `session/update` and
    /// any `session/request_permission`? No — permission requests have
    /// both `method` and `id`, so they are routed to `permissions`.
    notifications: Vec<Value>,
    /// Server → client requests (method + id) received before the
    /// response. Normally one `session/request_permission` per gated
    /// tool call.
    permissions: Vec<Value>,
}

/// Read frames until the frame with `want_id` arrives. A frame with a
/// `session/request_permission` method and its own `id` is a request
/// the bridge is waiting on; collect it into `permissions` and answer
/// it via `approve` (the caller's decision). Any other method+no-id
/// frame is a notification. A method+id frame that is NOT a permission
/// request is unexpected — the bridge makes only one kind of request —
/// so the test fails loudly rather than hanging.
async fn read_until_response<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    r: &mut BufReader<R>,
    w: &mut W,
    want_id: i64,
    approve: bool,
) -> ReadOutcome {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let mut notifications = Vec::new();
    let mut permissions = Vec::new();
    loop {
        let next = tokio::time::timeout_at(deadline, read_frame(r)).await;
        let frame = match next {
            Ok(Ok(Some(f))) => f,
            Ok(Ok(None)) => panic!("server closed before responding to id={want_id}"),
            Ok(Err(e)) => panic!("frame read error: {e}"),
            Err(_) => panic!("timed out waiting for response to id={want_id}"),
        };
        let has_id = frame.get("id").is_some();
        let method = frame.get("method").and_then(|m| m.as_str());

        // Response to our prompt request?
        if has_id
            && method.is_none()
            && frame.get("id").and_then(|v| v.as_i64()) == Some(want_id)
        {
            return ReadOutcome {
                response: frame,
                notifications,
                permissions,
            };
        }

        if has_id && method == Some("session/request_permission") {
            // Answer the permission request. The spec's shape: an
            // `outcome` object whose `outcome` field is "selected"
            // and whose `optionId` names the chosen option. The
            // bridge maps allow_once/allow_always -> Approve and
            // reject_once/reject_always -> Deny.
            let permission_id = frame["id"].as_i64().expect("permission id is i64");
            let option_id = if approve { "allow_once" } else { "reject_once" };
            let reply = json!({
                "jsonrpc": "2.0",
                "id": permission_id,
                "result": {
                    "outcome": {
                        "outcome": "selected",
                        "optionId": option_id,
                    }
                }
            });
            write_frame(w, &reply).await.expect("write permission reply");
            permissions.push(frame);
            continue;
        }

        if has_id && method.is_some() {
            panic!(
                "unexpected server->client request `{}`; bridge makes only \
                 session/request_permission; frame: {frame}",
                method.unwrap_or(""),
            );
        }

        notifications.push(frame);
    }
}

fn write_call(path: &str) -> Vec<ToolCall> {
    vec![ToolCall {
        id: Some("call-write-1".into()),
        tool_name: "write_file".into(),
        arguments: json!({ "path": path, "content": "fn main() {}\n" }),
    }]
}

/// Policy that makes `write_file` ASK: preset Yolo allows everything,
/// the per-tool override lifts `write_file` to `Ask` so the approval
/// branch fires.
fn ask_write_file_policy() -> PolicyEngine {
    let mut tools = std::collections::BTreeMap::new();
    tools.insert(
        "write_file".to_string(),
        ToolPolicy {
            mode: Some(Decision::Ask),
            ..ToolPolicy::default()
        },
    );
    let effective = Policy {
        preset: Preset::Yolo,
        tools,
        ..Policy::default()
    };
    PolicyEngine::from_effective_for_tests(effective)
}

async fn engine_with_policy(
    tmp: &tempfile::TempDir,
    policy: PolicyEngine,
) -> Arc<KodEngine> {
    let cfg = RouterConfig {
        embedder: None,
        skill_threshold: 0.3,
        working_dir: tmp.path().to_path_buf(),
        enable_memory: false,
        max_skills_per_query: 3,
        context_window: 8192,
        short_term_capacity: 100,
    };
    let engine = Arc::new(KodEngine::new(cfg, tmp.path().join("approval.redb")).unwrap());
    engine.set_policy(Arc::new(policy)).await;

    let script = vec![
        Script::Calls(write_call("out.txt")),
        Script::Text("done".into()),
    ];
    let provider = Arc::new(MockProvider::scripted("mock", script));
    let mut registry = kod_provider::ProviderRegistry::new();
    registry.insert(
        "mock",
        provider,
        kod_provider::ProviderCapabilities::conservative(),
        "mock-model",
    );
    let mut routing = kod_config::RoutingConfig::default();
    routing
        .by_task
        .insert("Simple".to_string(), "mock".to_string());
    engine
        .set_registry(
            Arc::new(registry),
            kod_provider::ModelRef::new("mock", "mock-model"),
            Some(routing),
        )
        .await;
    engine.start().await.unwrap();
    engine
}

/// The minimal handshake to get a `session_id` for the prompt. Reuses
/// the frame helpers. Ids 1 and 2.
async fn handshake<W: AsyncWrite + Unpin, R: AsyncRead + Unpin>(
    cw: &mut W,
    cr: &mut BufReader<R>,
) -> String {
    write_frame(
        cw,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "protocolVersion": 1 }
        }),
    )
    .await
    .unwrap();
    // initialize is never answered with a permission request, so a
    // no-op approve flag is fine.
    let _ = read_until_response(cr, cw, 1, true).await;

    write_frame(
        cw,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/new",
            "params": {}
        }),
    )
    .await
    .unwrap();
    let outcome = read_until_response(cr, cw, 2, true).await;
    outcome.response["result"]["sessionId"]
        .as_str()
        .expect("session/new must return a sessionId")
        .to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn approved_tool_call_writes_the_file_and_completes() {
    let tmp = tempfile::TempDir::new().unwrap();
    let engine = engine_with_policy(&tmp, ask_write_file_policy()).await;

    let (client_w, server_r) = tokio::io::duplex(64 * 1024);
    let (server_w, client_r) = tokio::io::duplex(64 * 1024);
    let serve_handle =
        tokio::spawn(
            async move { kod_core_serve::acp::serve_io(engine, server_r, server_w).await },
        );

    let mut cw = client_w;
    let mut cr = BufReader::new(client_r);

    let session_id = handshake(&mut cw, &mut cr).await;

    // Prompt: engine emits the tool call, hits Decision::Ask, sends
    // `session/request_permission`. Our helper answers allow_once.
    write_frame(
        &mut cw,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "session/prompt",
            "params": {
                "sessionId": session_id,
                "prompt": [{ "type": "text", "text": "write out.txt" }]
            }
        }),
    )
    .await
    .unwrap();

    let outcome = read_until_response(&mut cr, &mut cw, 3, /* approve */ true).await;

    // 1. The bridge asked exactly once.
    assert_eq!(
        outcome.permissions.len(),
        1,
        "one approval request expected; got {:?}",
        outcome.permissions,
    );
    // The request named the write_file tool.
    assert_eq!(
        outcome.permissions[0]["params"]["toolCall"]["kind"].as_str(),
        Some("edit"),
        "write_file maps to ACP kind=edit; got {}",
        outcome.permissions[0],
    );

    // 2. The turn completed normally (a denied call would produce a
    //    tool result and another round, but the model's second round
    //    is `done`, so stopReason is end_turn either way — the
    //    meaningful difference is whether the file was written).
    assert_eq!(
        outcome.response["result"]["stopReason"].as_str(),
        Some("end_turn"),
        "approved turn must complete; got {}",
        outcome.response,
    );

    // 3. The file exists — the approval was honored.
    assert!(
        tmp.path().join("out.txt").exists(),
        "approved write must land the file at out.txt",
    );

    // 4. The session/update stream carried a tool_call lifecycle, not
    //    just an agent_message_chunk. tool_call notifications are
    //    distinct from text chunks; a regression that routed the tool
    //    through the text path would fail here.
    let tool_updates: Vec<&Value> = outcome
        .notifications
        .iter()
        .filter(|n| n["method"].as_str() == Some("session/update"))
        .filter(|n| {
            matches!(
                n["params"]["update"]["sessionUpdate"].as_str(),
                Some("tool_call") | Some("tool_call_update"),
            )
        })
        .collect();
    assert!(
        !tool_updates.is_empty(),
        "the tool lifecycle must surface as tool_call notifications; got {:?}",
        outcome.notifications,
    );

    drop(cw);
    let serve_result = tokio::time::timeout(Duration::from_secs(5), serve_handle)
        .await
        .expect("serve_io exits within 5s of client EOF")
        .expect("serve_io must not panic");
    assert!(
        serve_result.is_ok(),
        "serve_io returned an error on clean EOF: {serve_result:?}",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn denied_tool_call_skips_the_file_and_still_completes() {
    let tmp = tempfile::TempDir::new().unwrap();
    let engine = engine_with_policy(&tmp, ask_write_file_policy()).await;

    let (client_w, server_r) = tokio::io::duplex(64 * 1024);
    let (server_w, client_r) = tokio::io::duplex(64 * 1024);
    let serve_handle =
        tokio::spawn(
            async move { kod_core_serve::acp::serve_io(engine, server_r, server_w).await },
        );

    let mut cw = client_w;
    let mut cr = BufReader::new(client_r);

    let session_id = handshake(&mut cw, &mut cr).await;

    write_frame(
        &mut cw,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "session/prompt",
            "params": {
                "sessionId": session_id,
                "prompt": [{ "type": "text", "text": "write out.txt" }]
            }
        }),
    )
    .await
    .unwrap();

    let outcome = read_until_response(&mut cr, &mut cw, 3, /* approve */ false).await;

    assert_eq!(
        outcome.permissions.len(),
        1,
        "one approval request expected even on denial; got {:?}",
        outcome.permissions,
    );
    assert_eq!(
        outcome.response["result"]["stopReason"].as_str(),
        Some("end_turn"),
        "a denied call is not a failed turn; got {}",
        outcome.response,
    );
    assert!(
        !tmp.path().join("out.txt").exists(),
        "denied write must NOT land the file at out.txt",
    );

    drop(cw);
    let serve_result = tokio::time::timeout(Duration::from_secs(5), serve_handle)
        .await
        .expect("serve_io exits within 5s of client EOF")
        .expect("serve_io must not panic");
    assert!(
        serve_result.is_ok(),
        "serve_io returned an error on clean EOF: {serve_result:?}",
    );
}
