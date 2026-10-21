//! ACP `session/cancel` notification (session 17, behavioral
//! integration).
//!
//! The handoff (session-16) named three remaining gaps; ACP
//! `session/cancel` was the highest-value. Reading the tests before
//! writing showed the engine side is well covered:
//!   - `tool_loop::test_cancel_stops_process` — pre-cancel.
//!   - `policy_gate::cancel_while_pending_approval_does_not_wait_the_timeout`
//!     — mid-approval cancel at the engine level.
//!
//! What neither covers is the **ACP bridge**:
//!   - `acp.rs:303-309` — the read-loop handler that dispatches
//!     `session/cancel` to `engine.request_cancel_for(engine_key(id))`.
//!   - `acp.rs:571` — the mapping from "message contains 'cancelled'"
//!     to `{stopReason: "cancelled"}` (vs "refusal" for other errors).
//!
//! Two tests:
//!   1. `session_cancel_during_pending_approval_returns_cancelled`
//!      — the ACP dispatch path, the engine_key construction, and the
//!        cancelled stop-reason mapping, driven end to end.
//!   2. `provider_error_maps_to_refusal_stop_reason`
//!      — the other side of the same match arm: a non-cancel error
//!        must map to "refusal", not "cancelled" and not "end_turn".
//!
//! No model, no network, no subprocess. The two helpers `write_frame`
//! and `read_frame` are duplicates of `acp_session.rs` / `acp_approval.rs`
//! (each `it` file is its own binary; small duplication is the
//! convention here).

use kod_config::{Decision, Policy, PolicyEngine, Preset, ToolPolicy};
use kod_core::KodEngine;
use kod_core::router::RouterConfig;
use kod_provider::testkit::{MockProvider, Script};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

/// Write one `Content-Length`-framed ACP message.
async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, msg: &Value) -> std::io::Result<()> {
    let body = serde_json::to_vec(msg).unwrap();
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    w.write_all(header.as_bytes()).await?;
    w.write_all(&body).await?;
    w.flush().await?;
    Ok(())
}

/// Read one `Content-Length`-framed ACP message. `Ok(None)` on clean
/// EOF.
async fn read_frame<R: AsyncRead + Unpin>(r: &mut BufReader<R>) -> std::io::Result<Option<Value>> {
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
    Ok(Some(
        serde_json::from_slice(&buf).expect("frame body is JSON"),
    ))
}

/// Read frames until one arrives with `want_id` and no `method`
/// (a response). Any frame with a `method` (request or notification)
/// is collected into `notifications` for the caller to inspect.
/// Bounded by a 20s wall-clock timeout so a hung bridge fails the
/// test instead of the harness.
async fn read_until_response<R: AsyncRead + Unpin>(
    r: &mut BufReader<R>,
    want_id: i64,
    notifications: &mut Vec<Value>,
) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let frame = match tokio::time::timeout_at(deadline, read_frame(r)).await {
            Ok(Ok(Some(f))) => f,
            Ok(Ok(None)) => panic!("server closed before responding to id={want_id}"),
            Ok(Err(e)) => panic!("frame read error: {e}"),
            Err(_) => panic!("timed out waiting for response to id={want_id}"),
        };
        let is_response = frame.get("id").and_then(|v| v.as_i64()) == Some(want_id)
            && frame.get("method").is_none();
        if is_response {
            return frame;
        }
        notifications.push(frame);
    }
}

/// Standard handshake: initialize (id 1), session/new (id 2). Returns
/// the ACP session id. Same pattern as `acp_session.rs::handshake`,
/// specialised for the two id-only frames we need here.
async fn handshake<W, R>(cw: &mut W, cr: &mut BufReader<R>) -> String
where
    W: AsyncWrite + Unpin,
    R: AsyncRead + Unpin,
{
    let mut ignore = Vec::new();

    write_frame(
        cw,
        &json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": { "protocolVersion": 1 }
        }),
    )
    .await
    .unwrap();
    let _ = read_until_response(cr, 1, &mut ignore).await;

    write_frame(
        cw,
        &json!({
            "jsonrpc": "2.0", "id": 2, "method": "session/new", "params": {}
        }),
    )
    .await
    .unwrap();
    let resp = read_until_response(cr, 2, &mut ignore).await;
    resp["result"]["sessionId"]
        .as_str()
        .expect("session/new must return a sessionId")
        .to_string()
}

/// Standard preset Yolo + per-tool override forcing `write_file` to
/// Ask. A write_file tool call parks on the approval gate, which is
/// the "in flight" state the cancel has to interrupt.
fn ask_write_file_policy() -> PolicyEngine {
    let mut tools = std::collections::BTreeMap::new();
    tools.insert(
        "write_file".to_string(),
        ToolPolicy {
            mode: Some(Decision::Ask),
            ..ToolPolicy::default()
        },
    );
    PolicyEngine::from_effective_for_tests(Policy {
        preset: Preset::Yolo,
        tools,
        ..Policy::default()
    })
}

/// Engine with the given policy, the given provider script, and a
/// mock provider registry wired to "Simple" -> mock.
async fn engine_with(
    tmp: &tempfile::TempDir,
    policy: PolicyEngine,
    script: Vec<Script>,
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
    let engine = Arc::new(KodEngine::new(cfg, tmp.path().join("cancel.redb")).unwrap());
    engine.set_policy(Arc::new(policy)).await;

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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_cancel_reaches_the_engine_and_maps_to_cancelled_stop_reason() {
    // Pre-cancel, not mid-turn: `session/cancel` is sent before
    // `session/prompt`, so the engine's cancel flag is set when the
    // prompt handler calls `process_streaming_for`. This is the
    // deterministic shape that exercises the whole ACP path:
    //   1. the read-loop dispatch (acp.rs:303) calls
    //      `engine.request_cancel_for(engine_key(session_id))`;
    //   2. `run_prompt` passes the SAME key to
    //      `process_streaming_for`;
    //   3. `process_streaming_for` observes the flag and returns
    //      `Err(...cancelled...)`;
    //   4. `run_prompt` maps the error message to
    //      `{stopReason: "cancelled"}` (acp.rs:571).
    //
    // The mid-turn ACP timing path (cancel while the approval gate is
    // held) needs a provider that blocks; the engine side of that is
    // covered by `policy_gate::cancel_while_pending_approval_does_not_wait_the_timeout`.
    // This test is about the *bridge*, not the engine.
    let tmp = tempfile::TempDir::new().unwrap();
    let engine = engine_with(
        &tmp,
        ask_write_file_policy(),
        vec![Script::Text("would-be reply".into())],
    )
    .await;

    let (client_w, server_r) = tokio::io::duplex(64 * 1024);
    let (server_w, client_r) = tokio::io::duplex(64 * 1024);
    let serve_handle =
        tokio::spawn(
            async move { kod_core_serve::acp::serve_io(engine, server_r, server_w).await },
        );

    let mut cw = client_w;
    let mut cr = BufReader::new(client_r);
    let session_id = handshake(&mut cw, &mut cr).await;

    // Cancel the session. Notifications have no `id`; the read loop
    // routes them to the notification branch and calls
    // `request_cancel_for` synchronously before reading the next
    // frame.
    write_frame(
        &mut cw,
        &json!({
            "jsonrpc": "2.0",
            "method": "session/cancel",
            "params": { "sessionId": session_id }
        }),
    )
    .await
    .unwrap();

    // Now prompt. The prompt handler's engine call sees the cancel
    // flag at round entry and returns an error whose message contains
    // "cancelled".
    write_frame(
        &mut cw,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "session/prompt",
            "params": {
                "sessionId": session_id,
                "prompt": [{ "type": "text", "text": "never mind" }]
            }
        }),
    )
    .await
    .unwrap();

    let mut notifications = Vec::new();
    let resp = read_until_response(&mut cr, 3, &mut notifications).await;

    // The stop-reason mapping, tested: an error whose message contains
    // "cancelled" must produce "cancelled", not "refusal", not
    // "end_turn", not an error response.
    assert_eq!(
        resp["result"]["stopReason"].as_str(),
        Some("cancelled"),
        "cancelled engine result must map to stopReason=cancelled; got {resp}",
    );
    // The prompt handler must respond, not error out. A regression
    // that changed `run_prompt` to respond_error on a cancel would
    // fail here (result missing).
    assert!(
        resp.get("error").is_none(),
        "cancelled turn must not surface as a JSON-RPC error; got {resp}",
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
async fn non_cancel_provider_error_maps_to_refusal_stop_reason() {
    // The other arm of acp.rs:571: an error whose message does NOT
    // contain "cancelled" must map to "refusal". Without this test,
    // a regression that made the "cancelled" branch too broad (e.g.
    // matching on any error, or on the empty string) would silently
    // label every provider failure as a user cancellation — and a
    // client that retries on refusal would skip the retry.
    let tmp = tempfile::TempDir::new().unwrap();
    let engine = engine_with(
        &tmp,
        ask_write_file_policy(),
        vec![Script::Error("upstream exploded".into())],
    )
    .await;

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
                "prompt": [{ "type": "text", "text": "anything" }]
            }
        }),
    )
    .await
    .unwrap();

    let mut notifications = Vec::new();
    let resp = read_until_response(&mut cr, 3, &mut notifications).await;

    assert_eq!(
        resp["result"]["stopReason"].as_str(),
        Some("refusal"),
        "a non-cancel provider error must map to stopReason=refusal; got {resp}",
    );
    assert!(
        resp.get("error").is_none(),
        "a provider failure is delivered as a stopReason, not as a JSON-RPC error; got {resp}",
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
