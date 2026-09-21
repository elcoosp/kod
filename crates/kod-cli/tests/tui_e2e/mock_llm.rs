//! A loopback OpenAI-compatible server for the E2E TUI tests, built
//! on axum 0.8.
//!
//! # Why a real HTTP server instead of a trait mock
//!
//! The TUI runs as a separate OS process. `terminal-testlib` spawns it
//! through a PTY, so there is no address space in which a `dyn
//! LlmProvider` stub could be injected. The only hermetic way to
//! intercept the model is to point `base_url` at a socket we control.
//!
//! That constraint is also the reason this is worth doing: the SSE
//! encoding, TCP chunk boundaries, and response parsing the provider
//! does are all exercised end-to-end.
//!
//! # What it serves
//!
//! - `GET  /v1/models`           — the registry's startup probe.
//! - `POST /v1/chat/completions` — the completion, streamed as SSE.
//!
//! Both handlers are `Fn` (not `FnOnce`): the per-request data
//! (reply text, byte-by-byte flag) lives in an `Arc<AppState>` that
//! the router holds via `with_state`, and each handler clones an
//! `Arc` out of the `State` extractor. A `move` closure that captures
//! `String` directly would be `FnOnce` and would silently fail after
//! the first request — the bug that made the earlier axum version
//! answer `GET /v1/models` but never `POST /v1/chat/completions`.

use std::convert::Infallible;
use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::{get, post};
use futures::stream::{self, Stream};
use tokio::sync::oneshot;

/// Canned reply the mock sends for every completion request.
pub const MOCK_REPLY: &str = "Hi from the mock";

/// Per-request configuration shared by the router's handlers.
#[derive(Clone)]
struct AppState {
    reply: String,
    byte_by_byte: bool,
}

/// A running mock server bound to a loopback port.
///
/// `Drop` signals the server to shut down and joins the runtime's
/// background thread, so a leaked listener cannot keep the test
/// process alive after the assertion fails.
pub struct MockServer {
    /// The OS-assigned loopback port. Build the endpoint `base_url`
    /// as `http://127.0.0.1:{port}/v1`.
    pub port: u16,
    _rt: tokio::runtime::Runtime,
    shutdown: Option<oneshot::Sender<()>>,
}

impl MockServer {
    pub fn start(reply: &str) -> Self {
        Self::start_with(reply, false)
    }

    /// Like [`start`], but streams the reply **one byte per SSE
    /// event**.
    ///
    /// Exercises the provider's UTF-8 boundary handling. A reply with
    /// a multi-byte character split across events will reconstruct
    /// exactly if the provider reassembles at the character level, or
    /// lose the second half if it decodes per chunk.
    pub fn start_byte_by_byte(reply: &str) -> Self {
        Self::start_with(reply, true)
    }

    fn start_with(reply: &str, byte_by_byte: bool) -> Self {
        // A multi-thread runtime with one worker so the server runs
        // on a background thread while the test thread drives the
        // PTY. A current-thread runtime would require the test
        // thread to be inside `block_on`, which it cannot be — it is
        // busy sending keystrokes.
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("build mock runtime");

        let (port_tx, port_rx) = std::sync::mpsc::channel::<u16>();
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

        let state = AppState {
            reply: reply.to_string(),
            byte_by_byte,
        };

        rt.spawn(async move {
            let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
                Ok(l) => l,
                Err(e) => {
                    eprintln!("mock: bind failed: {e}");
                    let _ = port_tx.send(0);
                    return;
                }
            };
            let port = listener.local_addr().expect("local_addr").port();
            eprintln!("mock: bound on 127.0.0.1:{port}");
            let _ = port_tx.send(port);

            // `Router::new()` with `.with_state(state)` — the
            // canonical axum 0.8 shape. Handlers extract
            // `State<AppState>`; the router clones the state into
            // each one, so the handler is a reusable `Fn`.
            //
            // The middleware runs for every request *before* routing
            // and logs the method + URI. It is the only way to see a
            // request that matches no route, or that axum rejects
            // before dispatch.
            let app = Router::new()
                .route("/v1/models", get(models))
                .route("/v1/chat/completions", post(chat_completions))
                .layer(axum::middleware::from_fn(log_request))
                .with_state(Arc::new(state));

            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.await;
                })
                .await;
        });

        let port = port_rx.recv().expect("mock did not report a port");
        assert!(port > 0, "mock failed to bind");

        Self {
            port,
            _rt: rt,
            shutdown: Some(shutdown_tx),
        }
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        // The runtime is dropped here; its background thread joins
        // when the runtime shuts down.
    }
}

/// `GET /v1/models` — the registry's startup probe.
///
/// Answers a one-entry list so startup does not log a spurious
/// warning that a test could mistake for a real failure.
async fn models() -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({
        "object": "list",
        "data": [{ "id": "mock-model", "object": "model" }]
    }))
}

/// `POST /v1/chat/completions` — the completion.
///
/// Extracts `State` (shared per-request config) and returns an SSE
/// stream of chunk events matching the OpenAI streaming shape:
/// opening role delta, content delta(s), terminal `finish_reason`
/// delta, then `[DONE]`.
async fn chat_completions(
    State(state): State<Arc<AppState>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    eprintln!("mock: serving chat completion");
    let events = sse_events(&state.reply, state.byte_by_byte);
    let stream = stream::iter(events.into_iter().map(Ok::<_, Infallible>));
    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// Build the SSE event list for `reply`.
///
/// Owned `Event`s are returned rather than a borrowed stream, so the
/// handler does not have to name a lifetime tied to `state.reply`.
fn sse_events(reply: &str, byte_by_byte: bool) -> Vec<Event> {
    let mut events = Vec::with_capacity(reply.len() + 4);

    // Opening chunk: role only. The OpenAI spec includes `role` in
    // the first delta and omits it afterwards.
    events.push(Event::default().data(chunk_json(Some("assistant"), None, None)));

    if byte_by_byte {
        // One byte per event, split on UTF-8 *bytes*.
        for b in reply.as_bytes() {
            let s = String::from_utf8_lossy(&[*b]).into_owned();
            events.push(Event::default().data(chunk_json(None, Some(&s), None)));
        }
    } else {
        // Two chunks so the accumulator's concatenation is exercised.
        let mid = reply.len() / 2;
        let (a, b) = reply.split_at(mid);
        events.push(Event::default().data(chunk_json(None, Some(a), None)));
        events.push(Event::default().data(chunk_json(None, Some(b), None)));
    }

    // Terminal chunk: empty delta, `finish_reason: "stop"`.
    events.push(Event::default().data(chunk_json(None, None, Some("stop"))));

    // `[DONE]` sentinel, per the OpenAI streaming spec.
    events.push(Event::default().data("[DONE]"));

    events
}

fn chunk_json(role: Option<&str>, content: Option<&str>, finish: Option<&str>) -> String {
    let mut delta = serde_json::Map::new();
    if let Some(r) = role {
        delta.insert("role".to_string(), serde_json::Value::String(r.to_string()));
    }
    if let Some(c) = content {
        delta.insert(
            "content".to_string(),
            serde_json::Value::String(c.to_string()),
        );
    }

    let value = serde_json::json!({
        "id": "chatcmpl-mock",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": "mock-model",
        "choices": [{
            "index": 0,
            "delta": delta,
            "finish_reason": finish,
        }]
    });
    serde_json::to_string(&value).expect("chunk json")
}


/// Log every incoming request before routing.
///
/// `from_fn` middleware has signature `Fn(Request, Next) -> Response`.
/// `Next::run(req)` must be awaited to hand the request to the next
/// layer — forgetting that call is the classic way to write
/// middleware that appears to run but hangs every request.
async fn log_request(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let method = req.method().clone();
    let uri = req.uri().clone();
    let headers = req.headers().clone();
    eprintln!("mock: in {method} {uri}");
    // Content-Length matters for the SSE POST: a chunked or truncated
    // body would explain a hang without a corresponding log line.
    if let Some(cl) = headers.get(axum::http::header::CONTENT_LENGTH) {
        eprintln!("mock:   content-length: {:?}", cl);
    }
    if let Some(ct) = headers.get(axum::http::header::CONTENT_TYPE) {
        eprintln!("mock:   content-type: {:?}", ct);
    }
    let mut resp = next.run(req).await;
    // Force the connection closed after every response. reqwest's
    // connection pool will otherwise reuse the GET's connection for
    // the subsequent POST, and if anything is holding that task open
    // the POST is stuck behind it. Closing is the simplest way to
    // guarantee a fresh connection per request — and the mock is
    // not perf-sensitive.
    resp.headers_mut().insert(
        axum::http::header::CONNECTION,
        axum::http::HeaderValue::from_static("close"),
    );
    eprintln!("mock: out {method} {uri} -> {}", resp.status());
    resp
}
