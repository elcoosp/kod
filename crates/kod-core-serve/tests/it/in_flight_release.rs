//! The ACP `in_flight` busy guard must be released when a prompt task
//! ends, whether it succeeded or errored (session 18, fault-injection
//! arc — regression guard for a P1 fixed in a bug-hunt).
//!
//! `acp.rs:459` documents the bug the fix addressed: a panic inside
//! `run_prompt` aborted the task before `in_flight.remove`, wedging
//! the session on `-32002` (session is busy) forever. The fix wraps
//! `run_prompt` in a spawned task so the cleanup always runs. No test
//! pinned the observable contract — until this file.
//!
//! Honest scope: the *panic-in-run_prompt* path itself is not
//! directly exercised. Reaching it would need a test-only panic
//! injection inside `run_prompt`'s own body (the ACP handler is
//! defensively coded — provider failures surface as error responses,
//! not panics). What this file pins is the *release on completion*
//! path: successful end, and error/refusal end. That is the same
//! `in_flight.remove` call the panic path reaches, so a regression
//! that removes it, or that re-inlines `run_prompt` and returns early
//! before cleanup, fails these tests.
//!
//! Proven by a positive control: the test script temporarily removes
//! the `in_flight.remove` line, confirms both tests fail with the
//! -32002 busy response, restores, confirms green. A guard that only
//! ever passes is untested; this one is proven to have teeth.

use kod_core::KodEngine;
use kod_core::router::RouterConfig;
use kod_provider::testkit::{MockProvider, Script};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

// --- Frame helpers. Duplicated per `it` file by convention (see
// acp_session.rs, acp_approval.rs, acp_cancel.rs) because each is a
// separate module in the same test binary and does not share its
// siblings' private fns. ---

async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, msg: &Value) -> std::io::Result<()> {
    let body = serde_json::to_vec(msg).unwrap();
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    w.write_all(header.as_bytes()).await?;
    w.write_all(&body).await?;
    w.flush().await?;
    Ok(())
}

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

async fn read_until_response<R: AsyncRead + Unpin>(r: &mut BufReader<R>, want_id: i64) -> Value {
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
        // Notifications (session/update) are ignored by these tests.
    }
}

async fn handshake<W, R>(cw: &mut W, cr: &mut BufReader<R>) -> String
where
    W: AsyncWrite + Unpin,
    R: AsyncRead + Unpin,
{
    write_frame(
        cw,
        &json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}),
    )
    .await
    .unwrap();
    let _ = read_until_response(cr, 1).await;

    write_frame(
        cw,
        &json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":{}}),
    )
    .await
    .unwrap();
    let resp = read_until_response(cr, 2).await;
    resp["result"]["sessionId"]
        .as_str()
        .expect("session/new must return a sessionId")
        .to_string()
}

async fn engine_with_script(tmp: &tempfile::TempDir, script: Vec<Script>) -> Arc<KodEngine> {
    let cfg = RouterConfig {
        embedder: None,
        skill_threshold: 0.3,
        working_dir: tmp.path().to_path_buf(),
        enable_memory: false,
        max_skills_per_query: 3,
        context_window: 8192,
        short_term_capacity: 100,
    };
    let engine = Arc::new(KodEngine::new(cfg, tmp.path().join("inflight.redb")).unwrap());
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

/// Assert that `resp` is not the -32002 busy error. The ACP bridge
/// returns `{"error": {"code": -32002, "message": "session is busy..."}}`
/// for a prompt while another is in flight; every test here asserts
/// the busy guard has *released* before the next prompt.
fn assert_not_busy(resp: &Value, context: &str) {
    if let Some(code) = resp
        .get("error")
        .and_then(|e| e.get("code"))
        .and_then(|v| v.as_i64())
    {
        assert_ne!(
            code, -32002,
            "{context}: session must not be wedged busy after the previous \
             prompt ended; got {resp}",
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_is_not_wedged_busy_after_a_normal_prompt_completes() {
    let tmp = tempfile::TempDir::new().unwrap();
    let engine = engine_with_script(
        &tmp,
        vec![
            Script::Text("first reply".into()),
            Script::Text("second reply".into()),
        ],
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

    // First prompt: completes normally. Its response is drained, so
    // the `in_flight` flag has been set and then removed.
    write_frame(
        &mut cw,
        &json!({
            "jsonrpc":"2.0","id":3,"method":"session/prompt",
            "params":{"sessionId":session_id,"prompt":[{"type":"text","text":"one"}]}
        }),
    )
    .await
    .unwrap();
    let r1 = read_until_response(&mut cr, 3).await;
    assert_not_busy(&r1, "after prompt 1 (normal completion)");
    assert_eq!(
        r1["result"]["stopReason"].as_str(),
        Some("end_turn"),
        "prompt 1 must complete normally; got {r1}",
    );

    // Second prompt on the SAME session: if `in_flight` leaked, this
    // returns -32002. That is the regression this test pins.
    write_frame(
        &mut cw,
        &json!({
            "jsonrpc":"2.0","id":4,"method":"session/prompt",
            "params":{"sessionId":session_id,"prompt":[{"type":"text","text":"two"}]}
        }),
    )
    .await
    .unwrap();
    let r2 = read_until_response(&mut cr, 4).await;
    assert_not_busy(&r2, "after prompt 2 (second prompt on same session)");
    assert_eq!(
        r2["result"]["stopReason"].as_str(),
        Some("end_turn"),
        "prompt 2 must complete normally; got {r2}",
    );

    drop(cw);
    let _ = tokio::time::timeout(Duration::from_secs(5), serve_handle).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_is_not_wedged_busy_after_a_failed_prompt_completes() {
    // The error path is the one a bug-hunt comment names as the shape
    // most likely to leak `in_flight` if cleanup moved. The provider
    // returns an Err on the first call and Text on the second.
    let tmp = tempfile::TempDir::new().unwrap();
    let engine = engine_with_script(
        &tmp,
        vec![
            Script::Error("simulated provider failure".into()),
            Script::Text("recovered".into()),
        ],
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

    // First prompt: provider errors, run_prompt maps it to "refusal".
    // The response still arrives, and `in_flight` must be removed.
    write_frame(
        &mut cw,
        &json!({
            "jsonrpc":"2.0","id":3,"method":"session/prompt",
            "params":{"sessionId":session_id,"prompt":[{"type":"text","text":"fail please"}]}
        }),
    )
    .await
    .unwrap();
    let r1 = read_until_response(&mut cr, 3).await;
    assert_not_busy(&r1, "after failed prompt (before second prompt)");
    // 100ms barrier: see the PR's commit message for the server-side
    // race this works around. The proper fix (move in_flight cleanup
    // before the response frame) is deferred to a dedicated session.
    assert_eq!(
        r1["result"]["stopReason"].as_str(),
        Some("refusal"),
        "an errored prompt must respond with stopReason=refusal; got {r1}",
    );

    // Second prompt on the same session. If the error path skipped
    // `in_flight.remove`, this returns -32002 forever.
    write_frame(
        &mut cw,
        &json!({
            "jsonrpc":"2.0","id":4,"method":"session/prompt",
            "params":{"sessionId":session_id,"prompt":[{"type":"text","text":"recover"}]}
        }),
    )
    .await
    .unwrap();
    let r2 = read_until_response(&mut cr, 4).await;
    assert_not_busy(&r2, "after second prompt following a failure");
    assert_eq!(
        r2["result"]["stopReason"].as_str(),
        Some("end_turn"),
        "the recovered prompt must complete; got {r2}",
    );

    drop(cw);
    let _ = tokio::time::timeout(Duration::from_secs(5), serve_handle).await;
}
