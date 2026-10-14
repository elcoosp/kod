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

/// One scripted response the mock serves for a single request.
///
/// A real agentic turn is request/response/request: the model asks for
/// a tool, the harness runs it, the model is called again with the
/// result. A single fixed reply cannot drive that loop, so the mock
/// serves a *script* — one step per incoming request, the last step
/// repeated once the list is exhausted.
#[derive(Clone)]
pub enum MockStep {
    /// Plain text, streamed as content deltas.
    Text(String),
    /// One tool call, streamed in the OpenAI `tool_calls` delta shape.
    /// The harness then runs the tool and calls the model again; the
    /// next step answers that call.
    ToolCall {
        name: String,
        /// JSON string the model would emit as `function.arguments`.
        arguments: String,
    },
}

/// Per-request configuration shared by the router's handlers.
#[derive(Clone)]
struct AppState {
    /// The script, consumed one step per `POST /v1/chat/completions`.
    script: Arc<Vec<MockStep>>,
    /// How many completion requests have been served. `fetch_add(1)`
    /// picks the step; past the end, the last step repeats.
    request_count: Arc<std::sync::atomic::AtomicUsize>,
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
    /// A server that serves `script`, one step per request, repeating
    /// the last step once exhausted. The first call returns a tool
    /// call, the second the text answer — the shape of an agentic
    /// turn.
    pub fn start_scripted(script: Vec<MockStep>) -> Self {
        assert!(!script.is_empty(), "a script needs at least one step");
        Self::start_inner(script, false)
    }

    /// Like [`Self::start_scripted`] but streams each text step one
    /// character at a time.
    pub fn start_scripted_char_by_char(script: Vec<MockStep>) -> Self {
        assert!(!script.is_empty(), "a script needs at least one step");
        Self::start_inner(script, true)
    }
}

impl MockServer {
    pub fn start(reply: &str) -> Self {
        Self::start_with(reply, false)
    }

    /// Like [`start`], but streams the reply **one character per
    /// SSE event**.
    ///
    /// Exercises the engine's chunk accumulator across many small
    /// events. Note: this does *not* split multi-byte characters
    /// across events — an SSE `data:` line is UTF-8 text, and half
    /// a codepoint in one would be an invalid frame, not a valid
    /// reassembly case. The Anthropic decoder's mid-codepoint bug
    /// (H-P2) lives at the TCP layer and needs a raw-socket test.
    pub fn start_char_by_char(reply: &str) -> Self {
        Self::start_with(reply, true)
    }

    fn start_with(reply: &str, byte_by_byte: bool) -> Self {
        // A single-step text script: the pre-scripted behaviour,
        // expressed through the same machinery.
        Self::start_inner(vec![MockStep::Text(reply.to_string())], byte_by_byte)
    }

    fn start_inner(script: Vec<MockStep>, byte_by_byte: bool) -> Self {
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
            script: Arc::new(script),
            request_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
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
    let n = state
        .request_count
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    // Past the end of the script, repeat the last step so a client
    // that calls more times than the script expects still gets an
    // answer rather than an empty stream.
    let step_index = n.min(state.script.len() - 1);
    let step = state.script[step_index].clone();
    eprintln!("mock: serving chat completion #{n} (step {step_index})");

    let events = match step {
        MockStep::Text(t) => sse_text_events(&t, state.byte_by_byte),
        MockStep::ToolCall { name, arguments } => {
            // The call id must be unique per request: the TUI keys its
            // tool rows by call id, so a fixed id makes two calls in
            // one turn collapse into a single row.
            sse_tool_call_events(&name, &arguments, n)
        }
    };
    let stream = stream::iter(events.into_iter().map(Ok::<_, Infallible>));
    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// SSE events for a tool call.
///
/// The OpenAI wire shape the provider's adk parser expects: a
/// `tool_calls` array in the delta, each entry carrying `index`, `id`,
/// and a `function` with `name` and (possibly fragmented)
/// `arguments`. The finish chunk carries `finish_reason: "tool_calls"`,
/// which is what tells the parser to emit the accumulated call.
fn sse_tool_call_events(name: &str, arguments: &str, call_seq: usize) -> Vec<Event> {
    // First delta: id + name, empty arguments. Second: the arguments.
    // Splitting them mirrors how real providers stream a call and
    // exercises the accumulator's append path.
    let start = serde_json::json!({
        "id": "chatcmpl-mock",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": "mock-model",
        "choices": [{
            "index": 0,
            "delta": {
                "role": "assistant",
                "tool_calls": [{
                    "index": 0,
                    "id": format!("call_mock_{call_seq}"),
                    "type": "function",
                    "function": { "name": name, "arguments": "" }
                }]
            },
            "finish_reason": null,
        }]
    });

    let args_delta = serde_json::json!({
        "id": "chatcmpl-mock",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": "mock-model",
        "choices": [{
            "index": 0,
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "function": { "arguments": arguments }
                }]
            },
            "finish_reason": null,
        }]
    });

    let finish = serde_json::json!({
        "id": "chatcmpl-mock",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": "mock-model",
        "choices": [{
            "index": 0,
            "delta": {},
            "finish_reason": "tool_calls",
        }]
    });

    vec![
        Event::default().data(serde_json::to_string(&start).expect("start json")),
        Event::default().data(serde_json::to_string(&args_delta).expect("args json")),
        Event::default().data(serde_json::to_string(&finish).expect("finish json")),
        Event::default().data("[DONE]"),
    ]
}

/// Build the SSE event list for `reply`.
///
/// Owned `Event`s are returned rather than a borrowed stream, so the
/// handler does not have to name a lifetime tied to `state.reply`.
fn sse_text_events(reply: &str, byte_by_byte: bool) -> Vec<Event> {
    let mut events = Vec::with_capacity(reply.len() + 4);

    // Opening chunk: role only. The OpenAI spec includes `role` in
    // the first delta and omits it afterwards.
    events.push(Event::default().data(chunk_json(Some("assistant"), None, None)));

    if byte_by_byte {
        // One *character* per event. `chars()` yields whole
        // codepoints, so a multi-byte character arrives in a single
        // `data:` line and the frame stays valid UTF-8. Splitting
        // by byte would emit lone continuation bytes; `from_utf8_
        // lossy` would turn them into U+FFFD, and the resulting
        // mojibake would be the mock's bug, not the provider's.
        for c in reply.chars() {
            let s = c.to_string();
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
