//! ACP `session/prompt` full-turn round-trip (session 16, behavioral
//! integration arc).
//!
//! `kod-cli/tests/it/acp_handshake.rs` proves the `initialize` handshake;
//! its doc-comment says so explicitly. Everything after — `session/new`,
//! `session/prompt`, the `session/update` notification stream, and the
//! final `{stopReason}` response — was untested end-to-end.
//!
//! This file drives the full turn in-process via `serve_io` (the
//! parameterized form of `serve` added in ac84e3e) over a
//! `tokio::io::duplex` pair, with a `MockProvider`-backed engine. No
//! subprocess, no model, deterministic.
//!
//! It also exercises the streaming contract the ACP bridge owes the
//! editor: text arrived incrementally as `agent_message_chunk`
//! notifications, and the final response carries only the stop reason
//! (the spec forbids echoing text back in the response).

use kod_core::KodEngine;
use kod_core::router::RouterConfig;
use kod_provider::testkit::{MockProvider, Script};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

/// Write one `Content-Length`-framed ACP message. Mirrors the private
/// `write_frame` in `kod_core_serve::acp` — duplicated here because
/// that helper is not exported, and the test must speak the wire
/// itself.
async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, msg: &Value) -> std::io::Result<()> {
    let body = serde_json::to_vec(msg).unwrap();
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    w.write_all(header.as_bytes()).await?;
    w.write_all(&body).await?;
    w.flush().await?;
    Ok(())
}

/// Read one `Content-Length`-framed ACP message. Returns `Ok(None)` on
/// clean EOF (the server side exits when its reader does).
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

/// Read frames until one carries `id`, collecting any notifications
/// into `notifications`. Times out so a hung server fails the test
/// instead of the harness.
async fn read_until_response<R: AsyncRead + Unpin>(
    r: &mut BufReader<R>,
    want_id: i64,
    notifications: &mut Vec<Value>,
) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let next = tokio::time::timeout_at(deadline, read_frame(r)).await;
        let frame = match next {
            Ok(Ok(Some(f))) => f,
            Ok(Ok(None)) => panic!("server closed the stream before responding to id={want_id}"),
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

/// Build a started engine whose provider answers every prompt with
/// `text`. The engine is what `serve_io` will drive.
async fn engine_with_text_reply(
    temp: &tempfile::TempDir,
    text: &str,
) -> Arc<KodEngine> {
    let cfg = RouterConfig {
        embedder: None,
        skill_threshold: 0.3,
        working_dir: temp.path().to_path_buf(),
        enable_memory: false,
        max_skills_per_query: 3,
        context_window: 8192,
        short_term_capacity: 100,
    };
    let engine = Arc::new(KodEngine::new(cfg, temp.path().join("acp.redb")).unwrap());

    let provider = Arc::new(MockProvider::new("mock", Script::Text(text.into())));
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
async fn full_session_prompt_round_trips_in_process() {
    let temp = tempfile::TempDir::new().unwrap();
    let engine = engine_with_text_reply(&temp, "hello from acp").await;

    // Two ends of the transport: the "client" side is ours, the
    // "server" side is fed to serve_io.
    let (client_w, server_r) = tokio::io::duplex(64 * 1024);
    let (server_w, client_r) = tokio::io::duplex(64 * 1024);

    let serve_handle = tokio::spawn(async move {
        kod_core_serve::acp::serve_io(engine, server_r, server_w).await
    });

    let mut cw = client_w;
    let mut cr = BufReader::new(client_r);
    let mut notifs: Vec<Value> = Vec::new();

    // --- initialize ---
    write_frame(
        &mut cw,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "protocolVersion": 1 }
        }),
    )
    .await
    .unwrap();
    let init_resp = read_until_response(&mut cr, 1, &mut notifs).await;
    assert!(
        init_resp["result"]["agentInfo"]["name"].as_str() == Some("kod"),
        "initialize must return agentInfo.name=kod; got {init_resp}",
    );

    // --- session/new ---
    write_frame(
        &mut cw,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/new",
            "params": {}
        }),
    )
    .await
    .unwrap();
    let new_resp = read_until_response(&mut cr, 2, &mut notifs).await;
    let session_id = new_resp["result"]["sessionId"]
        .as_str()
        .expect("session/new must return a sessionId")
        .to_string();
    assert!(
        session_id.starts_with("kod-"),
        "session id must carry the kod- prefix; got {session_id}",
    );

    // --- session/prompt ---
    notifs.clear();
    write_frame(
        &mut cw,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "session/prompt",
            "params": {
                "sessionId": session_id,
                "prompt": [
                    { "type": "text", "text": "say something" }
                ]
            }
        }),
    )
    .await
    .unwrap();
    let prompt_resp = read_until_response(&mut cr, 3, &mut notifs).await;

    // The spec requires the response to carry ONLY {stopReason: ...}.
    let result = &prompt_resp["result"];
    assert_eq!(
        result["stopReason"].as_str(),
        Some("end_turn"),
        "a normal turn must respond stopReason=end_turn; got {prompt_resp}",
    );

    // The text was streamed as agent_message_chunk notifications, in
    // order, before the response.
    let streamed: String = notifs
        .iter()
        .filter(|n| n["method"].as_str() == Some("session/update"))
        .filter(|n| {
            n["params"]["update"]["sessionUpdate"].as_str() == Some("agent_message_chunk")
        })
        .filter_map(|n| {
            n["params"]["update"]["content"]["text"]
                .as_str()
                .map(String::from)
        })
        .collect();
    assert!(
        streamed.contains("hello from acp"),
        "the adapter-parsed text must stream as agent_message_chunk; got {streamed:?}",
    );

    // Every session/update the bridge emitted must be scoped to the
    // session it was prompted on. Cross-session leakage is a class the
    // handshake test cannot catch.
    for n in &notifs {
        if n["method"].as_str() == Some("session/update") {
            assert_eq!(
                n["params"]["sessionId"].as_str(),
                Some(session_id.as_str()),
                "session/update must carry the prompted sessionId; got {n}",
            );
        }
    }

    // Shut the server down cleanly by dropping the client writer.
    drop(cw);
    let serve_result = tokio::time::timeout(Duration::from_secs(5), serve_handle)
        .await
        .expect("serve_io must exit within 5s of client EOF")
        .expect("serve_io must not panic");
    assert!(
        serve_result.is_ok(),
        "serve_io returned an error on clean EOF: {serve_result:?}",
    );
}
