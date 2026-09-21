//! A loopback OpenAI-compatible server for the E2E TUI tests.
//!
//! Why a real HTTP server instead of a trait mock: the TUI runs as a
//! separate OS process. `ratatui-testlib` spawns it through a PTY, so
//! there is no address space in which a `dyn LlmProvider` stub could
//! be injected. The only hermetic way to intercept the model is to
//! point `base_url` at a socket we control and answer on it.
//!
//! That constraint is also the reason this is worth doing: the SSE
//! encoding, TCP chunk boundaries, and response parsing the provider
//! does are all exercised end-to-end. A trait mock would skip every
//! one of those layers.
//!
//! The server is deliberately minimal — one route, one canned reply
//! — so a failure in a test points at the TUI, not at the mock.

use std::convert::Infallible;
use std::sync::mpsc;

use axum::Router;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::{get, post};
use futures::stream::{self, Stream};
use tokio::sync::oneshot;

/// Canned reply the mock sends for every completion request.
pub const MOCK_REPLY: &str = "Hi from the mock";

/// A running mock server bound to a loopback port.
///
/// `Drop` shuts the server down and joins the runtime. Tests keep one
/// of these alive for the duration of the test so the port stays
/// open.
pub struct MockServer {
    /// The OS-assigned loopback port. Build the endpoint `base_url`
    /// as `http://127.0.0.1:{port}/v1`.
    pub port: u16,
    _rt: tokio::runtime::Runtime,
    shutdown: Option<oneshot::Sender<()>>,
}

impl MockServer {
    /// Start a mock on `127.0.0.1:0` (OS-assigned port).
    ///
    /// `reply` is the text the assistant streams back. The default
    /// `MOCK_REPLY` is a fine choice for tests that only need "a
    /// reply arrived"; pass a custom string when the assertion greps
    /// for it in the screen.
    pub fn start(reply: &str) -> Self {
        Self::start_with(reply, false)
    }

    /// Like [`start`], but optionally streams the reply **one byte
    /// per SSE event**.
    #[allow(dead_code)]
    ///
    /// This exists to exercise the provider's UTF-8 boundary handling
    /// — the Anthropic split-UTF8 bug (H-P2) lived in a decoder that
    /// assumed each TCP chunk was a complete character. Byte-at-a-
    /// time SSE events reproduce that case for the OpenAI provider.
    pub fn start_byte_by_byte(reply: &str) -> Self {
        Self::start_with(reply, true)
    }

    fn start_with(reply: &str, byte_by_byte: bool) -> Self {
        // A multi-thread runtime with one worker so the server runs on
        // a background thread while the test thread drives the PTY.
        // A current-thread runtime would require the test thread to
        // be inside `block_on` — which it cannot be, because it is
        // busy sending keystrokes.
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("build mock runtime");

        let (port_tx, port_rx) = mpsc::channel::<u16>();
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

        let reply = reply.to_string();
        rt.spawn(async move {
            let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
                Ok(l) => l,
                Err(e) => {
                    let _ = port_tx.send(0);
                    eprintln!("mock_llm: bind failed: {e}");
                    return;
                }
            };
            let port = listener.local_addr().expect("local_addr").port();
            let _ = port_tx.send(port);

            let app = build_router(reply, byte_by_byte);
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
        // when the runtime shuts down. No explicit join needed.
    }
}

fn build_router(reply: String, byte_by_byte: bool) -> Router {
    Router::new()
        .route("/v1/chat/completions", post(move || {
            // Eagerly build the event list on the request thread so
            // the returned stream owns its data outright; no borrowed
            // `&reply` crosses the async boundary.
            let events = sse_events(&reply, byte_by_byte);
            async move { sse_from_events(events) }
        }))
        // The provider may probe `GET /v1/models` at registry build
        // time. Answer with a single-entry list so a startup probe
        // succeeds rather than logging a warning that could be
        // mistaken for a real failure.
        .route("/v1/models", get(|| async {
            axum::Json(serde_json::json!({
                "object": "list",
                "data": [{ "id": "mock-model", "object": "model" }]
            }))
        }))
}

/// Build the full SSE event list for `reply`.
///
/// Returning the events as an owned `Vec<String>` (rather than a
/// `Sse<impl Stream>`) means the caller never has to name a lifetime
/// for data borrowed from the request handler. `String` payloads move
/// into the stream; the handler owns them for the duration.
fn sse_events(reply: &str, byte_by_byte: bool) -> Vec<String> {
    let mut events: Vec<String> = Vec::new();

    // Opening chunk: role only. The OpenAI spec includes `role` in
    // the first delta and omits it afterwards; a parser that requires
    // it only on the first chunk is correctly exercised here.
    events.push(chunk_json(Some("assistant"), None, None));

    // Content chunks.
    if byte_by_byte {
        // One byte per event, split on UTF-8 *bytes* — the provider
        // must reassemble.
        for b in reply.as_bytes() {
            let s = String::from_utf8_lossy(&[*b]).into_owned();
            events.push(chunk_json(None, Some(&s), None));
        }
    } else {
        // Split the reply into two chunks to prove the accumulator
        // concatenates rather than replacing.
        let mid = reply.len() / 2;
        let (a, b) = reply.split_at(mid);
        events.push(chunk_json(None, Some(a), None));
        events.push(chunk_json(None, Some(b), None));
    }

    // Terminal chunk: empty delta, `finish_reason: "stop"`.
    events.push(chunk_json(None, None, Some("stop")));

    // `[DONE]` sentinel, per the OpenAI streaming spec.
    events.push("[DONE]".to_string());

    events
}

/// Turn an owned `Vec<String>` into the SSE stream axum serves.
fn sse_from_events(
    events: Vec<String>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = stream::iter(events.into_iter().map(|payload| {
        Ok::<_, Infallible>(Event::default().data(payload))
    }));
    Sse::new(stream).keep_alive(KeepAlive::default())
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
