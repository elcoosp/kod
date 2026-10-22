//! Shared loopback LLM mock: axum server on an ephemeral port, serving
//! `GET /v1/models` and `POST /v1/chat/completions` (both streaming SSE
//! and non-streaming JSON, branched on the request's `stream` field),
//! with a hit counter so a test can prove the mock was actually hit
//! rather than silently falling back to a default endpoint.
//!
//! Callers:
//! - `kod_prompt_mock.rs` — `run_prompt`, `run_streaming_prompt`
//! - `run_swarm_wrapper.rs` — `run_swarm` (uses `start_scripted`)
//!
//! NOT used by `tui_e2e::mock_llm`, which is PTY-specific with scripted
//! tool-call steps. Also not used by `admin_models_mock`, whose
//! `/v1/models` handler is configurable per test (empty list, filtered
//! list, etc.) — a different job from "answer a completion with text".
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::extract::State;
use axum::response::IntoResponse;
use axum::response::sse::{Event, Sse};
use axum::routing::{get, post};
use futures::StreamExt;

pub struct MockLlm {
    pub base_url: String,
    hits: Arc<AtomicUsize>,
}

impl MockLlm {
    /// A mock that answers every completion with `reply`.
    pub async fn start(reply: &str) -> Self {
        Self::start_scripted(vec![reply.to_string()]).await
    }

    /// A mock that serves `replies[i]` for the i-th completion request,
    /// repeating the last once exhausted. Useful for a multi-step
    /// scenario (decompose -> agent reply).
    pub async fn start_scripted(replies: Vec<String>) -> Self {
        assert!(!replies.is_empty(), "a script needs at least one reply");
        let hits = Arc::new(AtomicUsize::new(0));
        let state = Arc::new(AppState {
            script: Arc::new(replies),
            count: Arc::new(AtomicUsize::new(0)),
            hits: hits.clone(),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        let router = Router::new()
            .route("/v1/models", get(models))
            .route("/v1/chat/completions", post(chat_completions))
            .route("/chat/completions", post(chat_completions))
            .with_state(state);
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Self {
            base_url: format!("http://{}", addr),
            hits,
        }
    }

    /// Number of `POST /v1/chat/completions` requests served so far.
    pub fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }

    /// Write a `config.toml` under `cfg_dir` pointing the default
    /// endpoint at this mock. `extra_toml` is appended verbatim, so a
    /// caller can add `[swarm]` or `[memory]` blocks.
    pub fn write_config(&self, cfg_dir: &Path, extra_toml: &str) {
        std::fs::create_dir_all(cfg_dir).expect("create config dir");
        std::fs::write(
            cfg_dir.join("config.toml"),
            format!(
                "[llm]\n\
                 network_access = true\n\
                 \n\
                 [[llm.endpoints]]\n\
                 name = \"mock\"\n\
                 provider = \"openai-compatible\"\n\
                 base_url = \"{}\"\n\
                 model = \"test-model\"\n\
                 max_tokens = 128\n\
                 context_window = 8192\n\
                 \n\
                 {}\n",
                self.base_url, extra_toml,
            ),
        )
        .expect("write config.toml");
    }
}

struct AppState {
    script: Arc<Vec<String>>,
    count: Arc<AtomicUsize>,
    hits: Arc<AtomicUsize>,
}

async fn models(State(_state): State<Arc<AppState>>) -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({
        "object": "list",
        "data": [{"id": "test-model", "object": "model"}]
    }))
}

async fn chat_completions(
    State(state): State<Arc<AppState>>,
    body: axum::body::Bytes,
) -> axum::response::Response {
    state.hits.fetch_add(1, Ordering::SeqCst);
    let n = state.count.fetch_add(1, Ordering::SeqCst);
    let step = n.min(state.script.len() - 1);
    let reply = state.script[step].clone();

    let streaming = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.get("stream").and_then(|s| s.as_bool()))
        .unwrap_or(false);

    if streaming {
        sse_response(reply).into_response()
    } else {
        json_response(reply).into_response()
    }
}

fn json_response(reply: String) -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({
        "id": "mock-1",
        "object": "chat.completion",
        "created": 0,
        "model": "test-model",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": reply},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
    }))
}

fn sse_response(
    reply: String,
) -> Sse<impl futures::Stream<Item = Result<Event, std::convert::Infallible>>> {
    let chunk = serde_json::json!({
        "id": "mock-1",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": "test-model",
        "choices": [{
            "index": 0,
            "delta": {"role": "assistant", "content": reply},
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
