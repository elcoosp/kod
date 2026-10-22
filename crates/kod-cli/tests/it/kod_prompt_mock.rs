//! `run_prompt` and `run_streaming_prompt` end-to-end against a mock
//! chat-completions endpoint.
//!
//! The `_inner` functions are already tested with `MockProvider`
//! (`prompt_mock.rs`); the wrappers are not, because they build
//! `EngineBootstrapOptions` internally and do not accept a
//! `provider_override`. The only way to exercise them is to point the
//! config's `base_url` at an HTTP endpoint the test controls.
//!
//! # The two environment redirections, both load-bearing
//!
//! - `KOD_CONFIG_DIR` — so `KodConfig::load_default()` reads the test's
//!   `config.toml` (with `base_url` pointing at the mock) instead of
//!   anything under the developer's home.
//! - `HOME` — the wrappers pass `db_path: None` to `engine_from_config`,
//!   which then uses `dirs::home_dir()/.kod/data/kod.redb` directly.
//!   No config key intercepts that path (unlike `run_serve`, which
//!   passes `db_path: Some(config.memory_db_path()?)` and so honours
//!   `[memory] long_term_db_path`). Overriding `HOME` moves every
//!   `~/.kod/...` default — DB, sessions, fixtures — into the tempdir.
//!
//! Each test is `#[serial]` because `HOME` and `KOD_CONFIG_DIR` are
//! process-global.
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::extract::State;
use axum::response::IntoResponse;
use axum::response::sse::{Event, Sse};
use axum::routing::{get, post};
use kod_cli::commands::prompt::{run_prompt, run_streaming_prompt};
use serial_test::serial;

struct EnvGuard {
    key: &'static str,
    prev: Option<std::ffi::OsString>,
}

impl EnvGuard {
    fn set(key: &'static str, value: &std::path::Path) -> Self {
        let prev = std::env::var_os(key);
        // Safety: `#[serial]` on every caller ensures exclusivity.
        unsafe {
            std::env::set_var(key, value);
        }
        EnvGuard { key, prev }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // Safety: same invariant as `set`.
        unsafe {
            match &self.prev {
                Some(prev) => std::env::set_var(self.key, prev),
                None => std::env::remove_var(self.key),
            }
        }
    }
}

struct AppState {
    reply: Arc<String>,
    hits: Arc<AtomicUsize>,
}

/// `GET /v1/models` — the registry's startup probe.
async fn models(State(_state): State<Arc<AppState>>) -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({
        "object": "list",
        "data": [{"id": "test-model", "object": "model"}]
    }))
}

/// Non-streaming chat completion: one JSON body.
async fn chat_completions_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    state.hits.fetch_add(1, Ordering::SeqCst);
    axum::Json(serde_json::json!({
        "id": "mock-1",
        "object": "chat.completion",
        "created": 0,
        "model": "test-model",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": state.reply.as_str()},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
    }))
}

/// Streaming chat completion: SSE deltas, then `[DONE]`.
async fn chat_completions_sse_handler(
    State(state): State<Arc<AppState>>,
) -> Sse<impl futures::Stream<Item = Result<Event, std::convert::Infallible>>> {
    use futures::StreamExt;
    state.hits.fetch_add(1, Ordering::SeqCst);
    let chunk = serde_json::json!({
        "id": "mock-1",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": "test-model",
        "choices": [{
            "index": 0,
            "delta": {"role": "assistant", "content": state.reply.as_str()},
            "finish_reason": null
        }]
    });
    let done = serde_json::json!({
        "id": "mock-1",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": "test-model",
        "choices": [{
            "index": 0,
            "delta": {},
            "finish_reason": "stop"
        }]
    });
    let events = vec![
        Ok(Event::default().data(chunk.to_string())),
        Ok(Event::default().data(done.to_string())),
        Ok(Event::default().data("[DONE]")),
    ];
    Sse::new(futures::stream::iter(events).boxed())
}

/// Start a mock on an ephemeral port. Returns the base_url (without
/// `/v1`; `normalize_base_url` in the provider appends it) and a hit
/// counter so a test can prove the mock was reached.
async fn spawn_mock(streaming: bool, reply: &str) -> (String, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let state = Arc::new(AppState {
        reply: Arc::new(reply.to_string()),
        hits: hits.clone(),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let router = if streaming {
        Router::new()
            .route("/v1/models", get(models))
            .route("/v1/chat/completions", post(chat_completions_sse_handler))
            .route("/chat/completions", post(chat_completions_sse_handler))
            .with_state(state)
    } else {
        Router::new()
            .route("/v1/models", get(models))
            .route("/v1/chat/completions", post(chat_completions_handler))
            .route("/chat/completions", post(chat_completions_handler))
            .with_state(state)
    };
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    (format!("http://{}", addr), hits)
}

/// Write the `config.toml`. `provider = "openai-compatible"` (hyphen)
/// is the valid spelling; `openai_compatible` (underscore) causes the
/// whole `[[llm.endpoints]]` block to be dropped and the engine falls
/// back to the Ollama default at `localhost:11434`.
fn write_config(cfg_dir: &std::path::Path, base_url: &str) {
    std::fs::create_dir_all(cfg_dir).expect("create config dir");
    std::fs::write(
        cfg_dir.join("config.toml"),
        format!(
            r#"[llm]
network_access = true

[[llm.endpoints]]
name = "mock"
provider = "openai-compatible"
base_url = "{base_url}"
model = "test-model"
max_tokens = 128
context_window = 8192
"#
        ),
    )
    .expect("write config.toml");
}

// ---- run_prompt ----------------------------------------------------

/// The non-streaming wrapper. Asserts both that the call returns Ok
/// and that the mock was actually hit — the second assertion is the
/// one that catches a config that silently fell back to a default
/// endpoint.
#[serial]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_prompt_hits_the_mock_endpoint() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (base_url, hits) = spawn_mock(false, "hello from the mock").await;
    write_config(tmp.path(), &base_url);
    let _home = EnvGuard::set("HOME", tmp.path());
    let _cfg = EnvGuard::set("KOD_CONFIG_DIR", tmp.path());
    let _sess = EnvGuard::set("KOD_SESSIONS_DIR", tmp.path());

    run_prompt("hi".to_string(), None, true, false)
        .await
        .expect("run_prompt against the mock must be Ok");

    assert!(
        hits.load(Ordering::SeqCst) >= 1,
        "the mock's /v1/chat/completions handler must have run — \
         zero hits means the config fell back to a default endpoint",
    );
}

/// Empty prompt: the wrapper's own guard fires before any HTTP call.
/// Asserts both the error text and that the mock was NOT hit.
#[serial]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_prompt_rejects_empty_input() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (base_url, hits) = spawn_mock(false, "should not be reached").await;
    write_config(tmp.path(), &base_url);
    let _home = EnvGuard::set("HOME", tmp.path());
    let _cfg = EnvGuard::set("KOD_CONFIG_DIR", tmp.path());
    let _sess = EnvGuard::set("KOD_SESSIONS_DIR", tmp.path());

    let err = run_prompt("   ".to_string(), None, true, false)
        .await
        .expect_err("an empty prompt must be an error");
    assert!(
        err.to_string().contains("empty prompt"),
        "unexpected error: {err}",
    );
    assert_eq!(
        hits.load(Ordering::SeqCst),
        0,
        "the empty-prompt guard must fire before any HTTP request",
    );
}

// ---- run_streaming_prompt ------------------------------------------

/// The streaming wrapper. Same two assertions: Ok return, and the
/// mock's SSE handler was actually hit.
#[serial]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_streaming_prompt_hits_the_mock_endpoint() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (base_url, hits) = spawn_mock(true, "streaming hello").await;
    write_config(tmp.path(), &base_url);
    let _home = EnvGuard::set("HOME", tmp.path());
    let _cfg = EnvGuard::set("KOD_CONFIG_DIR", tmp.path());
    let _sess = EnvGuard::set("KOD_SESSIONS_DIR", tmp.path());

    run_streaming_prompt("hi".to_string(), None)
        .await
        .expect("run_streaming_prompt against the mock must be Ok");

    assert!(
        hits.load(Ordering::SeqCst) >= 1,
        "the mock's /v1/chat/completions handler must have run",
    );
}

/// Streaming wrapper's empty-prompt guard.
#[serial]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_streaming_prompt_rejects_empty_input() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (base_url, hits) = spawn_mock(true, "should not be reached").await;
    write_config(tmp.path(), &base_url);
    let _home = EnvGuard::set("HOME", tmp.path());
    let _cfg = EnvGuard::set("KOD_CONFIG_DIR", tmp.path());
    let _sess = EnvGuard::set("KOD_SESSIONS_DIR", tmp.path());

    let err = run_streaming_prompt("   ".to_string(), None)
        .await
        .expect_err("an empty prompt must be an error");
    assert!(
        err.to_string().contains("empty prompt"),
        "unexpected error: {err}",
    );
    assert_eq!(
        hits.load(Ordering::SeqCst),
        0,
        "the empty-prompt guard must fire before any HTTP request",
    );
}
