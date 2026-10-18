//! Engine <-> real provider adapter seam (session 16, behavioral
//! integration arc).
//!
//! Every other `kod-core/tests/it/*` test injects a fake
//! `LlmProvider` (MockProvider / NopProvider / FailingProvider) at the
//! trait boundary. That proves the engine handles the
//! `CompletionRequest` / `GenerationResponse` structs correctly; it
//! does NOT prove the engine's request survives serialization into
//! what a real adapter sends, or that the adapter's parsed stream is
//! the shape the engine's dispatcher consumes.
//!
//! These tests close that seam: a real `OpenAICompatProvider` talks to
//! an `httpmock` server, and a real `KodEngine` drives it end to end.
//! No model, no budget, deterministic.
//!
//! The three tests exercise the three shapes the seam can take:
//!
//! 1. `engine_processes_a_real_adapter_text_response` — non-streaming
//!    `process()` through the real adapter, response text surfaces.
//! 2. `engine_request_reaches_the_wire_with_system_and_user` — the
//!    mock only answers if the request body contains both the system
//!    identity line and the user text, proving the engine's
//!    `CompletionRequest` is what the adapter actually PUT on the
//!    socket.
//! 3. `engine_streaming_through_real_adapter_delivers_text` — the
//!    engine's streaming path consumes an SSE body parsed by the real
//!    adapter's stream parser.

use httpmock::prelude::*;
use kod_core::{KodEngine, RouterConfig};
use kod_provider::{ModelRef, ProviderCapabilities, ProviderRegistry};
use kod_provider_openai::OpenAICompatProvider;
use std::sync::Arc;
use tempfile::TempDir;

fn engine_in(dir: &std::path::Path) -> KodEngine {
    let cfg = RouterConfig {
        embedder: None,
        skill_threshold: 0.3,
        working_dir: dir.to_path_buf(),
        enable_memory: false,
        max_skills_per_query: 3,
        context_window: 8192,
        short_term_capacity: 100,
    };
    KodEngine::new(cfg, dir.join("db.redb")).unwrap()
}

/// The minimal OpenAI chat-completion success body. Mirrors
/// `kod-provider-openai/tests/it/contracts.rs::simple_text_response`.
fn text_completion_body(text: &str) -> String {
    format!(
        r#"{{
  "id": "chatcmpl-seam",
  "object": "chat.completion",
  "choices": [{{
    "index": 0,
    "message": {{"role": "assistant", "content": "{text}"}},
    "finish_reason": "stop"
  }}],
  "usage": {{"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}}
}}"#
    )
}

/// One text delta followed by the `[DONE]` terminator, the shape the
/// adapter's streaming parser consumes.
fn text_sse_body(text: &str) -> String {
    format!(
        "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{text}\"}}}}]}}\n\ndata: [DONE]\n\n"
    )
}

/// Build an engine whose single provider is a real OpenAICompatProvider
/// pointed at `mock.base_url()`.
async fn engine_with_real_adapter(
    temp: &TempDir,
    mock: &MockServer,
    model_name: &str,
) -> KodEngine {
    let engine = engine_in(temp.path());
    let provider = Arc::new(
        OpenAICompatProvider::with_api_key(mock.base_url(), model_name, "test-key")
            .expect("provider construction"),
    );

    let mut registry = ProviderRegistry::new();
    registry.insert(
        "primary",
        provider.clone(),
        ProviderCapabilities::conservative(),
        model_name,
    );

    let mut routing = kod_config::RoutingConfig::default();
    routing
        .by_task
        .insert("Simple".to_string(), "primary".to_string());

    engine
        .set_registry(
            Arc::new(registry),
            ModelRef::new("primary", model_name),
            Some(routing),
        )
        .await;

    engine.start().await.expect("engine start");
    engine
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn engine_processes_a_real_adapter_text_response() {
    let mock = MockServer::start_async().await;
    let endpoint = mock
        .mock_async(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200)
                .header("content-type", "application/json")
                .body(text_completion_body("seam reply"));
        })
        .await;

    let temp = TempDir::new().unwrap();
    let engine = engine_with_real_adapter(&temp, &mock, "seam-model").await;

    let resp = engine.process("hello").await.expect("process");
    assert_eq!(
        resp.text.as_deref(),
        Some("seam reply"),
        "engine must surface the adapter-parsed text",
    );
    assert!(
        endpoint.calls_async().await >= 1,
        "the real adapter must have hit the wire",
    );

    let _ = engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn engine_request_reaches_the_wire_with_system_and_user() {
    // The mock only answers a POST whose body contains BOTH the
    // engine's identity/system line and the user turn. If the engine's
    // CompletionRequest is not what the real adapter serializes onto
    // the socket, httpmock returns 404 and `process` fails.
    let mock = MockServer::start_async().await;
    let _endpoint = mock
        .mock_async(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .body_includes("\"role\":\"system\"")
                .body_includes("\"role\":\"user\"")
                .body_includes("wire-probe-user-text");
            then.status(200)
                .header("content-type", "application/json")
                .body(text_completion_body("ok"));
        })
        .await;

    let temp = TempDir::new().unwrap();
    let engine = engine_with_real_adapter(&temp, &mock, "wire-model").await;

    let resp = engine
        .process("wire-probe-user-text")
        .await
        .expect("process must reach the mock's matcher");
    assert_eq!(resp.text.as_deref(), Some("ok"));

    let _ = engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn engine_streaming_through_real_adapter_delivers_text() {
    let mock = MockServer::start_async().await;
    let _endpoint = mock
        .mock_async(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200)
                .header("content-type", "text/event-stream")
                .body(text_sse_body("streamed seam text"));
        })
        .await;

    let temp = TempDir::new().unwrap();
    let engine = engine_with_real_adapter(&temp, &mock, "stream-model").await;

    // process_streaming takes an mpsc::Sender<String> and returns a
    // TaskResponse; the chunk stream arrives on the receiver. Same
    // pattern as the existing tool_loop::test_process_streaming_*
    // test. Dropping the sender after the call closes the channel so
    // the drain terminates.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(64);
    let response = engine
        .process_streaming("hello", &tx)
        .await
        .expect("process_streaming");
    drop(tx);

    let mut collected = String::new();
    while let Some(chunk) = rx.recv().await {
        collected.push_str(&chunk);
    }

    assert!(
        collected.contains("streamed seam text"),
        "engine's streaming path must surface the adapter-parsed text; got {collected:?}",
    );
    assert_eq!(
        response.text.as_deref(),
        Some("streamed seam text"),
        "the aggregated TaskResponse must carry the same text",
    );

    let _ = engine.shutdown().await;
}
