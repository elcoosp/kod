//! Unix-socket daemon (D6.2).
//!
//! # Why a daemon
//!
//! Two terminals, one agent. The first runs `kod serve`; every
//! subsequent `kod prompt --remote` or `kod chat --remote` from the
//! same user and the same working directory attaches to the same
//! engine — same transcript, same model, same tool loop. A fresh
//! second terminal does not start a cold engine and pay the
//! index/loading cost a second time.
//!
//! # Transport
//!
//! One Unix socket per user, at `$XDG_RUNTIME_DIR/kod.sock` on Linux
//! (a per-user tmpfs that the OS cleans on logout) or
//! `~/.kod/run/kod.sock` when `$XDG_RUNTIME_DIR` is unset (macOS).
//! Permissions are `0600` from the moment the socket exists, and
//! every accepted connection is checked for the peer's UID before a
//! single byte is read from it.
//!
//! **No TCP.** Not on loopback, not on a private interface. A
//! loopback TCP socket is reachable from every process on the host,
//! so its access control is the network stack's, not the OS's.
//! A Unix socket with a peer-UID check is the only mechanism this
//! daemon uses; the code contains no `TcpListener` and never will.
//!
//! # Protocol
//!
//! Newline-delimited JSON, one request per line, responses as
//! separate NDJSON lines. Every response carries the `id` of the
//! request it answers, so a client can multiplex requests on one
//! connection.
//!
//! Request:
//! ```json
//! {"v":1,"id":"r1","method":"process_streaming",
//!  "params":{"input":"hello","transcript_key":""}}
//! ```
//!
//! Responses (in order):
//! ```json
//! {"id":"r1","type":"chunk","data":"hi "}
//! {"id":"r1","type":"chunk","data":"there"}
//! {"id":"r1","type":"done","data":{ ...TaskResponse... }}
//! ```
//!
//! An error at any point:
//! ```json
//! {"id":"r1","type":"error","data":{"message":"..."}}
//! ```
//!
//! # Methods
//!
//! | Method                 | Params                                          |
//! |------------------------|-------------------------------------------------|
//! | `hello`                | `{min, max}` — version negotiation              |
//! | `process`              | `{input, transcript_key?}`                      |
//! | `process_streaming`    | `{input, transcript_key?}`                      |
//! | `steer`                | `{note, transcript_key?}`                       |
//! | `cancel`               | `{transcript_key?}`                             |
//! | `respond_to_approval`  | `{id, decision}`                                |
//! | `respond_to_question`  | `{id, answer}`                                  |
//! | `swarm`                | `{goal, max_agents?, merge?}`                   |
//! | `peek_session`         | `{key, max_chars?}` — non-attaching read        |
//! | `list_models`          | `{}`                                            |
//! | `set_model`            | `{endpoint, model}`                             |
//! | `shutdown`             | `{}`                                            |
//!
//! `PROTOCOL_METHODS` below is the same list, for a client author to
//! read as data rather than parse out of this table.
//!
//! # Lifecycle
//!
//! One server per user. `serve` binds the socket and refuses to
//! start if the file already exists and a live process is on it —
//! a stale socket from a crashed daemon is cleaned first. On
//! `shutdown` (or `SIGINT`) the accept loop exits, the socket file
//! is removed, and the call returns.

use kod_core::engine::KodEngine;
use kod_error::{KodError, Result};
use kod_provider::ModelRef;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Notify;

// T5-split: `default_socket_path` moved to `kod-core-state`
// so both kod-core and kod-core-serve can depend on it without a
// circular dependency. Re-exported here so existing call sites
// (`kod_core::serve::default_socket_path`) keep resolving.
pub use kod_core_state::socket_path::default_socket_path;

/// Protocol version. Sent by the client in every request; the
/// server ignores a missing version (older clients) but reserves
/// the right to reject a higher one in a future release.
pub const PROTOCOL_VERSION: u8 = 1;

/// The oldest protocol version this server still speaks.
pub const MIN_PROTOCOL_VERSION: u8 = 1;

/// Every method the protocol accepts, as strings.
///
/// For a client author and for the module doc table. This is *not* a
/// check on the dispatch: a `match` on `&str` cannot be verified
/// against a const without refactoring the match into a table, and a
/// test claiming to would be lying. A method listed here that the
/// dispatch does not handle is a server bug a reviewer catches; an
/// unknown method at runtime lands in the `other =>` arm and gets a
/// named error, which is the behavior that matters.
pub const PROTOCOL_METHODS: &[&str] = &[
    "cancel",
    "hello",
    "list_models",
    "peek_session",
    "process",
    "process_streaming",
    "respond_to_approval",
    "respond_to_question",
    "set_model",
    "shutdown",
    "steer",
    "swarm",
];

/// The newest protocol version this server speaks. A client asking
/// for a version above this is running ahead of the server; one below
/// `MIN` is behind it. Both get a named error rather than a confusing
/// failure deeper in the dispatch.
pub const MAX_PROTOCOL_VERSION: u8 = 1;

/// Default socket path for this user.

/// A request line.
#[derive(Debug, Deserialize)]
struct Request {
    /// Protocol version, read by the dispatch loop's version gate
    /// (`MIN_PROTOCOL_VERSION..=MAX_PROTOCOL_VERSION`). `None` for a
    /// pre-version client; every v1 method works without it.
    #[serde(default)]
    v: Option<u8>,
    id: String,
    method: String,
    #[serde(default)]
    params: Value,
}

/// One response line. The `data` field is method-dependent: a string
/// for `chunk`, a serialized `TaskResponse` for `done`, an object
/// with a `message` for `error`.
#[derive(Debug, Serialize)]
struct Response<'a> {
    id: &'a str,
    #[serde(rename = "type")]
    kind: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
}

/// Run the daemon until `shutdown` fires or `SIGINT` arrives.
pub async fn serve(engine: Arc<KodEngine>, socket_path: PathBuf) -> Result<()> {
    let listener = crate::transport::bind(&socket_path).await?;
    set_socket_perms(&socket_path)?;
    tracing::info!(socket = %socket_path.display(), "kod serve listening");

    let shutdown = Arc::new(Notify::new());
    let shutdown_for_signal = shutdown.clone();

    // SIGINT also shuts the daemon down — a user who started `kod
    // serve` in a terminal expects Ctrl+C to stop it.
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            tracing::info!("SIGINT received; shutting down");
            shutdown_for_signal.notify_one();
        }
    });

    loop {
        tokio::select! {
            accept = listener.accept() => {
                match accept {
                    Ok(stream) => {
                        let engine = engine.clone();
                        let shutdown = shutdown.clone();
                        let handler = tokio::spawn(async move {
                            if let Err(e) =
                                handle_connection(stream, engine, shutdown).await
                            {
                                tracing::debug!(error = %e, "connection ended");
                            }
                        });
                        // A panic in the connection handler used to be
                        // silent: the JoinHandle was dropped, so a
                        // JoinError never reached anything that could
                        // log it. The watcher turns an abnormal end
                        // (panic or cancellation) into an ERROR line
                        // rather than leaving "why did the daemon
                        // log a connection for 3 seconds and then
                        // nothing" unanswered.
                        tokio::spawn(async move {
                            if let Err(join_err) = handler.await {
                                tracing::error!(
                                    error = %join_err,
                                    "serve: connection handler ended abnormally (panic or cancellation)",
                                );
                            }
                        });
                    }
                    Err(e) => tracing::warn!(error = %e, "accept failed"),
                }
            }
            _ = shutdown.notified() => {
                tracing::info!("shutdown requested; stopping listener");
                break;
            }
        }
    }

    // Best-effort cleanup: the socket inode is created by us and
    // represents us, so removing it is the right thing on exit.
    let _ = std::fs::remove_file(&socket_path);
    Ok(())
}

/// Connect to a running daemon and ask it to shut down. Returns
/// `Ok(())` whether or not the daemon answered — a daemon that died
/// before processing the request is not an error from the caller's
/// point of view.
pub async fn stop_daemon(socket_path: &Path) -> Result<()> {
    let stream = crate::transport::connect(socket_path).await?;
    let req = serde_json::json!({
        "v": PROTOCOL_VERSION,
        "id": "stop-1",
        "method": "shutdown",
        "params": {},
    });
    let mut line = serde_json::to_string(&req).map_err(|e| KodError::Internal(e.to_string()))?;
    line.push('\n');
    let (read_half, mut write_half) = stream.into_split();
    write_half.write_all(line.as_bytes()).await?;
    write_half.flush().await?;
    // Read until the daemon closes the connection or answers. The
    // socket being unlinked is the true signal, which the CLI's
    // `kod serve --stop` polls for separately.
    let mut reader = BufReader::new(read_half);
    let mut buf = String::new();
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        reader.read_line(&mut buf),
    )
    .await;
    Ok(())
}

// ---------------------------------------------------------------------------
// Internal
// ---------------------------------------------------------------------------

/// `0600` on the socket file. The peer-UID check makes this
/// redundant at the protocol layer, but filesystem permissions are
/// what a `ls -la` inspection shows and what a user expects.
#[cfg(unix)]
fn set_socket_perms(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(path, perms).map_err(KodError::Io)
}

#[cfg(not(unix))]
fn set_socket_perms(_path: &Path) -> Result<()> {
    Ok(())
}

/// One connection's lifetime. Reads lines, dispatches methods,
/// writes NDJSON responses.
///
/// # Concurrency
///
/// Every outgoing line goes through one `mpsc` channel drained by a
/// single writer task, so a streaming response and a
/// `respond_to_approval` sent on the same connection cannot
/// interleave mid-line. `process_streaming` and `swarm` spawn a
/// task; the read loop returns immediately, so a client that
/// receives an approval marker mid-stream can send its answer
/// while the engine is paused on the matching oneshot.
/// H-R4: a `lines()` iterator with a per-line byte cap. Any line
/// that exceeds the cap kills the connection — a same-UID client
/// that wrote a giant unterminated line is either buggy or hostile;
/// neither deserves unbounded memory from the daemon.
struct CappedLines<'a, R: tokio::io::AsyncBufRead + Unpin> {
    reader: &'a mut R,
    cap: usize,
}

impl<'a, R: tokio::io::AsyncBufRead + Unpin> CappedLines<'a, R> {
    fn new(reader: &'a mut R, cap: usize) -> Self {
        Self { reader, cap }
    }

    async fn next_line(&mut self) -> Result<Option<String>> {
        use tokio::io::AsyncBufReadExt;
        // Read until newline, but cap the amount buffered. The
        // `read_until` future does not take a bound, so we detect
        // over-cap by checking the buffer length after the read and
        // (best-effort) close the connection if it is oversized.
        //
        // A more surgical bound would chunk the read manually; for
        // the shapes this daemon sees, the 1 MiB cap is several
        // orders of magnitude above any legitimate request, and
        // `BufReader::read_until` already grows in bounded steps,
        // so the buffer never allocates more than roughly (cap +
        // largest single chunk) before the check fires.
        // M-19: enforce the cap DURING the read. `read_until` appends
        // without bound until it sees the delimiter, so checking after
        // still OOMs on a newline-less stream.
        let mut buf = Vec::new();
        loop {
            let available = self.reader.fill_buf().await.map_err(KodError::Io)?;
            if available.is_empty() {
                break;
            }
            let take = available
                .iter()
                .position(|&b| b == b'\n')
                .map_or(available.len(), |p| p + 1);
            // The cap is a *content* budget: a trailing `\n` is a
            // terminator, not content. Pre-fix the check used `take`
            // as-is, so a line of exactly `cap` content bytes
            // followed by `\n` was rejected while the same content
            // at EOF was accepted. Measure content only.
            let terminator_len = if available.get(take.saturating_sub(1)) == Some(&b'\n') {
                1
            } else {
                0
            };
            let content_take = take - terminator_len;
            if buf.len() + content_take > self.cap {
                return Err(KodError::InvalidParameters {
                    reason: format!("request line exceeds {} bytes", self.cap),
                });
            }
            buf.extend_from_slice(&available[..take]);
            self.reader.consume(take);
            if buf.last() == Some(&b'\n') {
                break;
            }
        }
        if buf.is_empty() {
            return Ok(None);
        }
        if buf.last() == Some(&b'\n') {
            buf.pop();
        }
        if buf.last() == Some(&b'\r') {
            buf.pop();
        }
        Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
    }
}

pub async fn serve_io<R, W>(
    engine: Arc<KodEngine>,
    input: R,
    writer: W,
    shutdown: Arc<Notify>,
) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    // H-R4: cap the request line length. The pre-fix read used
    // `BufReader::lines()` with no bound: a same-UID client could
    // OOM the daemon by writing a giant line with no `\n`. 1 MiB is
    // several times any legitimate request frame.
    const MAX_REQUEST_LINE_BYTES: usize = 1024 * 1024;
    let mut reader = tokio::io::BufReader::new(input);
    let mut lines = CappedLines::new(&mut reader, MAX_REQUEST_LINE_BYTES);

    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<String>(256);
    let writer_task = tokio::spawn(async move {
        let mut writer = writer;
        while let Some(line) = out_rx.recv().await {
            if writer.write_all(line.as_bytes()).await.is_err() {
                break;
            }
            if writer.flush().await.is_err() {
                break;
            }
        }
    });

    while let Some(line) = match lines.next_line().await {
        Ok(Some(l)) => Some(l),
        Ok(None) => None,
        Err(e) => return Err(e),
    } {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let req: Request = match serde_json::from_str(trimmed) {
            Ok(r) => r,
            Err(e) => {
                // Bad request frame. A failed send here would have
                // been silent; log it so a closed writer channel is
                // visible in the logs rather than as a client waiting
                // on a reply that will never arrive. `id: ""` is the
                // JSON-RPC convention for a request that could not be
                // parsed far enough to extract an id.
                if let Err(write_err) = send_response(
                    &out_tx,
                    &Response {
                        id: "",
                        kind: "error",
                        data: Some(serde_json::json!({"message": format!("bad request: {e}")})),
                    },
                )
                .await
                {
                    tracing::warn!(
                        error = %write_err,
                        "serve: could not deliver the bad-request error frame",
                    );
                }
                continue;
            }
        };

        // Version gate. A client that declares a version outside the
        // server's range gets a named error here rather than an
        // opaque parse failure or a silently-wrong result deeper in
        // the dispatch. A missing `v` is an older client that
        // predates the field; it is allowed through, since every
        // version-1 method works without it.
        if let Some(v) = req.v
            && !(MIN_PROTOCOL_VERSION..=MAX_PROTOCOL_VERSION).contains(&v)
        {
            write_error(
                &out_tx,
                &req.id,
                &format!(
                    "unsupported protocol version {v}; this server speaks \
                     {MIN_PROTOCOL_VERSION}..={MAX_PROTOCOL_VERSION}",
                ),
            )
            .await?;
            continue;
        }

        match req.method.as_str() {
            "hello" => {
                // Explicit negotiation: the client declares the range
                // it can speak, the server answers with its own and
                // the overlap. A client whose range does not intersect
                // the server's gets `compatible: false` and can
                // disconnect with a clear reason instead of failing on
                // the first real request.
                let client_min = req
                    .params
                    .get("min")
                    .and_then(|v| v.as_u64())
                    .map(|n| n.min(u8::MAX as u64) as u8)
                    .unwrap_or(1);
                // Bug-hunt: a client sending `max = 300` used to be
                // truncated to `44` (300 as u8), breaking version
                // negotiation silently. The `min` handler two lines
                // above already does this correctly; mirror it here.
                let client_max = req
                    .params
                    .get("max")
                    .and_then(|v| v.as_u64())
                    .map(|n| n.min(u8::MAX as u64) as u8)
                    .unwrap_or(client_min);
                let lo = client_min.max(MIN_PROTOCOL_VERSION);
                let hi = client_max.min(MAX_PROTOCOL_VERSION);
                let compatible = lo <= hi;
                let data = serde_json::json!({
                    "server_min": MIN_PROTOCOL_VERSION,
                    "server_max": MAX_PROTOCOL_VERSION,
                    "client_min": client_min,
                    "client_max": client_max,
                    "compatible": compatible,
                    "use": if compatible { Some(hi) } else { None },
                });
                write_ok(&out_tx, &req.id, data).await?
            }
            "process" => {
                // H-R4: spawn the non-streaming prompt so the read
                // loop can continue serving `cancel` / `steer` /
                // `respond_to_approval` frames on the same connection
                // while the model generates. The pre-fix inline
                // `.await` blocked the loop for the length of the
                // whole generation.
                let engine = engine.clone();
                let out = out_tx.clone();
                let id = req.id.clone();
                let input = string_param(&req.params, "input");
                let key = string_param(&req.params, "transcript_key");
                let out_for_guard = out.clone();
                let id_for_guard = id.clone();
                spawn_panic_guarded(
                    async move {
                        match engine.process_for(&key, &input).await {
                            Ok(resp) => {
                                if let Err(write_err) = write_done(&out, &id, &resp).await {
                                    tracing::warn!(
                                        error = %write_err,
                                        "serve: could not deliver the process_for done frame",
                                    );
                                }
                            }
                            Err(e) => {
                                // The engine errored; the client needs to
                                // know. The pre-fix `let _ =` would silently
                                // drop the error frame if the writer channel
                                // had closed, leaving the caller waiting
                                // until its own timeout fired (if any).
                                if let Err(write_err) = write_error(&out, &id, &e.to_string()).await
                                {
                                    tracing::warn!(
                                        error = %write_err,
                                        engine_error = %e,
                                        "serve: could not deliver the process_for error frame",
                                    );
                                }
                            }
                        }
                    },
                    out_for_guard,
                    id_for_guard,
                );
            }
            "process_streaming" => {
                let engine = engine.clone();
                let out = out_tx.clone();
                let id = req.id.clone();
                let input = string_param(&req.params, "input");
                let key = string_param(&req.params, "transcript_key");
                let out_for_guard = out.clone();
                let id_for_guard = id.clone();
                spawn_panic_guarded(
                    async move {
                        if let Err(e) = run_streaming(&engine, &out, &id, &input, &key).await {
                            // The streaming pump failed mid-turn. Try to
                            // hand the client an error frame; a failure to
                            // deliver *that* is a second, independent
                            // problem and is logged rather than dropped.
                            if let Err(write_err) = send_response(
                                &out,
                                &Response {
                                    id: &id,
                                    kind: "error",
                                    data: Some(serde_json::json!({"message": e.to_string()})),
                                },
                            )
                            .await
                            {
                                tracing::warn!(
                                    error = %write_err,
                                    engine_error = %e,
                                    "serve: could not deliver the process_streaming error frame",
                                );
                            }
                        }
                    },
                    out_for_guard,
                    id_for_guard,
                );
            }
            "steer" => {
                let note = string_param(&req.params, "note");
                let key = string_param(&req.params, "transcript_key");
                engine.steer_for(&key, &note).await;
                write_ack(&out_tx, &req.id).await?;
            }
            "cancel" => {
                let key = string_param(&req.params, "transcript_key");
                engine.request_cancel_for(&key);
                write_ack(&out_tx, &req.id).await?;
            }
            "respond_to_approval" => {
                let item_id = req.params.get("id").and_then(|v| v.as_u64());
                let decision = match req.params.get("decision").and_then(|v| v.as_str()) {
                    Some("approve") => Some(kod_core::engine::ApprovalDecision::Approve),
                    Some("deny") => Some(kod_core::engine::ApprovalDecision::Deny),
                    Some("deny_always") => Some(kod_core::engine::ApprovalDecision::DenyAlways),
                    _ => None,
                };
                match (item_id, decision) {
                    (Some(n), Some(d)) => {
                        let delivered = engine.respond_to_approval(n, d).await;
                        write_ok(
                            &out_tx,
                            &req.id,
                            serde_json::json!({"delivered": delivered}),
                        )
                        .await?;
                    }
                    _ => {
                        write_error(
                            &out_tx,
                            &req.id,
                            "respond_to_approval requires 'id' (u64) and \
                             'decision' (approve|deny|deny_always)",
                        )
                        .await?;
                    }
                }
            }
            "respond_to_question" => {
                let item_id = req.params.get("id").and_then(|v| v.as_u64());
                let answer = string_param(&req.params, "answer");
                match item_id {
                    Some(n) => {
                        let delivered = engine.respond_to_question(n, answer).await;
                        write_ok(
                            &out_tx,
                            &req.id,
                            serde_json::json!({"delivered": delivered}),
                        )
                        .await?;
                    }
                    None => {
                        write_error(&out_tx, &req.id, "respond_to_question requires 'id' (u64)")
                            .await?;
                    }
                }
            }
            "swarm" => {
                let goal = string_param(&req.params, "goal");
                if goal.trim().is_empty() {
                    write_error(&out_tx, &req.id, "swarm: 'goal' is required").await?;
                    continue;
                }
                let max_agents = req
                    .params
                    .get("max_agents")
                    .and_then(|v| v.as_u64())
                    .map(|n| n as usize)
                    .unwrap_or(5);
                let merge = req
                    .params
                    .get("merge")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);
                let engine = engine.clone();
                let out = out_tx.clone();
                let id = req.id.clone();
                let out_for_guard = out.clone();
                let id_for_guard = id.clone();
                spawn_panic_guarded(
                    async move {
                        if let Err(e) =
                            run_swarm(&engine, &out, &id, &goal, max_agents, merge).await
                        {
                            if let Err(write_err) = send_response(
                                &out,
                                &Response {
                                    id: &id,
                                    kind: "error",
                                    data: Some(serde_json::json!({"message": e.to_string()})),
                                },
                            )
                            .await
                            {
                                tracing::warn!(
                                    error = %write_err,
                                    engine_error = %e,
                                    "serve: could not deliver the swarm error frame",
                                );
                            }
                        }
                    },
                    out_for_guard,
                    id_for_guard,
                );
            }
            "peek_session" => {
                // A read-only view of another transcript. An empty
                // `key` is the interactive session; `swarm:<id>` is
                // an agent. Nothing is attached, nothing is mutated —
                // which is the point: a client previewing a session
                // must not disturb it.
                let key = string_param(&req.params, "key");
                let max_chars = req
                    .params
                    .get("max_chars")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(4000) as usize;
                let data = engine.peek_transcript(&key, max_chars).await;
                write_ok(&out_tx, &req.id, data).await?
            }
            "shutdown" => {
                write_ack(&out_tx, &req.id).await?;
                shutdown.notify_one();
                break;
            }
            "list_models" => match engine.list_models().await {
                Ok(models) => {
                    // Populate the per-model context-window catalog so
                    // a subsequent `set_model` on this endpoint can
                    // allocate against the model's real window rather
                    // than the endpoint default.
                    let current = engine.current_model().await;
                    engine.record_model_catalog(&current.endpoint, &models);
                    // Wire compat: the protocol's `models` field is a
                    // flat list of id strings. ModelInfo carries more
                    // (context window, pricing) but the client and the
                    // existing tooling read bare names.
                    let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
                    let data = serde_json::json!({ "models": ids });
                    write_ok(&out_tx, &req.id, data).await?
                }
                Err(e) => write_error(&out_tx, &req.id, &e.to_string()).await?,
            },
            "set_model" => {
                let endpoint = string_param(&req.params, "endpoint");
                let model = string_param(&req.params, "model");
                engine
                    .set_current_model(ModelRef::new(endpoint, model))
                    .await;
                write_ack(&out_tx, &req.id).await?;
            }
            other => {
                write_error(
                    &out_tx,
                    &req.id,
                    &format!(
                        "unknown method: {other} (server speaks protocol \
                     {MIN_PROTOCOL_VERSION}..={MAX_PROTOCOL_VERSION})",
                    ),
                )
                .await?;
            }
        }
    }

    // Reader is done. Drop our own sender, then wait a bounded time
    // for the writer task to drain what spawned pumps have already
    // queued. Without the timeout a hung streaming task would keep
    // the connection handler alive forever.
    drop(out_tx);
    // Bug-hunt: the pre-fix shape dropped the timeout result, so a
    // hung spawned pump (a streaming task whose provider never
    // returned) was indistinguishable from a clean drain. Log the
    // timeout so a "why did my connection close mid-stream?" report
    // has an answer in the logs.
    match tokio::time::timeout(std::time::Duration::from_secs(5), writer_task).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "serve: writer task panicked or was cancelled");
        }
        Err(_) => {
            tracing::warn!(
                "serve: writer task did not drain within 5s; \
                 a spawned pump is likely still holding a sender",
            );
        }
    }
    Ok(())
}

async fn handle_connection(
    stream: crate::transport::Stream,
    engine: Arc<KodEngine>,
    shutdown: Arc<Notify>,
) -> Result<()> {
    let (read_half, write_half) = stream.into_split();
    serve_io(engine, read_half, write_half, shutdown).await
}

/// Pump one streaming call's chunks through the writer channel.
/// Extracted from the main loop so `process_streaming` can spawn it.
async fn run_streaming(
    engine: &Arc<KodEngine>,
    out: &tokio::sync::mpsc::Sender<String>,
    req_id: &str,
    input: &str,
    key: &str,
) -> Result<()> {
    let (chunk_tx, mut chunk_rx) = tokio::sync::mpsc::channel::<String>(64);
    let engine_clone = engine.clone();
    let input_owned = input.to_string();
    let key_owned = key.to_string();
    let call = tokio::spawn(async move {
        engine_clone
            .process_streaming_for(&key_owned, &input_owned, &chunk_tx)
            .await
    });
    // BUG-C-05: fresh clones for the cancel-on-disconnect path. The
    // spawn above moved `engine_clone`/`key_owned` into its async
    // block; these are separate clones for the pump.
    let engine_for_cancel = engine.clone();
    let key_for_cancel = key.to_string();
    while let Some(chunk) = chunk_rx.recv().await {
        if write_chunk(out, req_id, &chunk).await.is_err() {
            // The client is gone: cancel the turn. Without this, the
            // spawned engine task runs to completion with no consumer
            // — tools execute, files get written, tokens get billed.
            engine_for_cancel.request_cancel_for(&key_for_cancel);
            call.abort();
            return Err(KodError::Internal("client disconnected mid-stream".into()));
        }
    }
    let outcome = call
        .await
        .map_err(|e| KodError::Internal(format!("engine task panicked: {e}")))?;
    match outcome {
        Ok(resp) => write_done(out, req_id, &resp).await?,
        Err(e) => write_error(out, req_id, &e.to_string()).await?,
    }
    Ok(())
}

/// Same shape as `run_streaming`, for the `swarm` method.
async fn run_swarm(
    engine: &Arc<KodEngine>,
    out: &tokio::sync::mpsc::Sender<String>,
    req_id: &str,
    goal: &str,
    max_agents: usize,
    merge: bool,
) -> Result<()> {
    let runner =
        kod_core::swarm_runner::SwarmRunner::new(engine.clone(), max_agents, merge).await?;
    let (evt_tx, mut evt_rx) =
        tokio::sync::mpsc::channel::<kod_core::swarm_runner::SwarmEvent>(256);
    let goal_owned = goal.to_string();
    let run_handle = tokio::spawn(async move { runner.run(&goal_owned, &evt_tx).await });
    while let Some(evt) = evt_rx.recv().await {
        let data = serde_json::to_value(&evt).unwrap_or(serde_json::Value::Null);
        send_response(
            out,
            &Response {
                id: req_id,
                kind: "swarm_event",
                data: Some(data),
            },
        )
        .await?;
    }
    let outcome = run_handle
        .await
        .map_err(|e| KodError::Internal(format!("swarm task panicked: {e}")))?;
    match outcome {
        Ok(resp) => {
            let data = serde_json::json!({
                "merged": resp.merged,
                "merged_by_model": resp.merged_by_model,
                "conflicts": resp.conflicts.iter().map(|c| {
                    serde_json::json!({"file": c.file, "agents": c.agents})
                }).collect::<Vec<_>>(),
            });
            write_ok(out, req_id, data).await?;
        }
        Err(e) => write_error(out, req_id, &e.to_string()).await?,
    }
    Ok(())
}

fn string_param(params: &Value, key: &str) -> String {
    params
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

/// Serialize one response and send it through the writer channel.
async fn send_response(
    tx: &tokio::sync::mpsc::Sender<String>,
    response: &Response<'_>,
) -> Result<()> {
    let mut s =
        serde_json::to_string(response).map_err(|e| KodError::Serialization(e.to_string()))?;
    s.push('\n');
    tx.send(s)
        .await
        .map_err(|_| KodError::Internal("writer channel closed".to_string()))
}

async fn write_chunk(tx: &tokio::sync::mpsc::Sender<String>, id: &str, chunk: &str) -> Result<()> {
    send_response(
        tx,
        &Response {
            id,
            kind: "chunk",
            data: Some(Value::String(chunk.to_string())),
        },
    )
    .await
}

async fn write_done(
    tx: &tokio::sync::mpsc::Sender<String>,
    id: &str,
    resp: &kod_core::router::TaskResponse,
) -> Result<()> {
    let data = serde_json::to_value(resp).unwrap_or(Value::Null);
    send_response(
        tx,
        &Response {
            id,
            kind: "done",
            data: Some(data),
        },
    )
    .await
}

/// Spawn `fut` with panic isolation.
///
/// A bare `tokio::spawn(fut)` is the pre-fix shape: a panic unwinds
/// past the JoinHandle, no error frame reaches the client, and the
/// caller waits on its request id until its own timeout. The fix is
/// the shape `acp.rs:464` uses for `run_prompt` (aa21bf8): an inner
/// `tokio::spawn` whose `JoinHandle::await` returns `Err` on panic,
/// so the error path runs on normal, error, and panic.
pub(crate) fn spawn_panic_guarded<F>(fut: F, out: tokio::sync::mpsc::Sender<String>, id: String)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        let handle = tokio::spawn(fut);
        if let Err(join_err) = handle.await {
            let msg = if join_err.is_panic() {
                "serve: internal task panicked; the request could not be completed"
            } else {
                "serve: internal task was cancelled; the request could not be completed"
            };
            tracing::error!(
                req_id = %id,
                error = %join_err,
                "serve: task ended abnormally; sending an error frame",
            );
            let _ = write_error(&out, &id, msg).await;
        }
    });
}

async fn write_error(
    tx: &tokio::sync::mpsc::Sender<String>,
    id: &str,
    message: &str,
) -> Result<()> {
    send_response(
        tx,
        &Response {
            id,
            kind: "error",
            data: Some(serde_json::json!({ "message": message })),
        },
    )
    .await
}

async fn write_ack(tx: &tokio::sync::mpsc::Sender<String>, id: &str) -> Result<()> {
    send_response(
        tx,
        &Response {
            id,
            kind: "done",
            data: None,
        },
    )
    .await
}

async fn write_ok(tx: &tokio::sync::mpsc::Sender<String>, id: &str, data: Value) -> Result<()> {
    send_response(
        tx,
        &Response {
            id,
            kind: "done",
            data: Some(data),
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_socket_path_is_a_socket_path() {
        let p = kod_core_state::socket_path::default_socket_path();
        assert!(p.ends_with("kod.sock"), "got {p:?}");
    }

    #[test]
    fn response_serializes_with_kind_not_type_field() {
        let r = Response {
            id: "r1",
            kind: "chunk",
            data: Some(Value::String("hi".to_string())),
        };
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("\"type\":\"chunk\""), "got {s}");
        assert!(s.contains("\"id\":\"r1\""));
        assert!(s.contains("\"data\":\"hi\""));
    }

    #[test]
    fn ack_omits_data() {
        let r = Response {
            id: "r1",
            kind: "done",
            data: None,
        };
        let s = serde_json::to_string(&r).unwrap();
        assert!(!s.contains("data"), "ack should omit data: {s}");
    }

    #[test]
    fn request_parses_with_and_without_version() {
        let with: Request = serde_json::from_str(r#"{"v":1,"id":"a","method":"process"}"#).unwrap();
        assert_eq!(with.method, "process");
        let without: Request = serde_json::from_str(r#"{"id":"a","method":"process"}"#).unwrap();
        assert_eq!(without.method, "process");
    }
}

#[cfg(test)]
mod coverage_serve_serde {
    //! The daemon's wire shape is the contract any `kod
    //! * --remote` client depends on. A regression that renamed
    //! `type` to `kind` (or vice versa) breaks every client
    //! silently — the client skips unknown response types and the
    //! session appears to hang.
    use super::*;

    #[test]
    fn request_parses_with_every_documented_field() {
        let json = r#"{"v":1,"id":"r1","method":"process","params":{"input":"hi","transcript_key":"chat"}}"#;
        let r: Request = serde_json::from_str(json).unwrap();
        assert_eq!(r.id, "r1");
        assert_eq!(r.method, "process");
        assert_eq!(r.params["input"], "hi");
        assert_eq!(r.params["transcript_key"], "chat");
    }

    #[test]
    fn request_parses_without_version_field() {
        // Older clients and ad-hoc test scripts omit `v`; the
        // `#[serde(default)]` on the field is what keeps the
        // daemon compatible with them.
        let json = r#"{"id":"r1","method":"shutdown"}"#;
        let r: Request = serde_json::from_str(json).unwrap();
        assert_eq!(r.method, "shutdown");
        assert!(r.params.is_null(), "missing params must default to null");
    }

    #[test]
    fn request_requires_id_and_method() {
        // The daemon routes responses by id; a frame without one
        // cannot be answered. Missing either field is a hard
        // parse error.
        assert!(serde_json::from_str::<Request>(r#"{"method":"process"}"#).is_err());
        assert!(serde_json::from_str::<Request>(r#"{"id":"r1"}"#).is_err());
    }

    #[test]
    fn chunk_response_serializes_data_as_a_string() {
        let r = Response {
            id: "r1",
            kind: "chunk",
            data: Some(serde_json::Value::String("hi".to_string())),
        };
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("\"type\":\"chunk\""), "got {s}");
        assert!(s.contains("\"data\":\"hi\""), "got {s}");
    }

    #[test]
    fn done_response_with_no_data_omits_the_field() {
        // `skip_serializing_if = "Option::is_none"` on `data`
        // keeps an ack frame small. A regression that emitted
        // `"data":null` would still parse on the client, but the
        // change is worth pinning.
        let r = Response {
            id: "r1",
            kind: "done",
            data: None,
        };
        let s = serde_json::to_string(&r).unwrap();
        assert!(!s.contains("data"), "ack should omit data: {s}");
        assert!(s.contains("\"type\":\"done\""));
    }

    #[test]
    fn error_response_carries_a_message_object() {
        // The client reads `data.message`; a regression that
        // emitted the message as a bare string would make every
        // error response render as "(no message)".
        let r = Response {
            id: "r1",
            kind: "error",
            data: Some(serde_json::json!({"message": "boom"})),
        };
        let s = serde_json::to_string(&r).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["type"], "error");
        assert_eq!(v["data"]["message"], "boom");
    }

    #[test]
    fn protocol_version_is_one() {
        // The daemon and the clients agree on this constant.
        // Bumping it is a breaking wire change.
        assert_eq!(PROTOCOL_VERSION, 1);
    }

    #[test]
    fn default_socket_path_ends_with_the_expected_name() {
        // The daemon and the CLI both derive the path through
        // this function; a change to the filename would break
        // every `--remote` invocation with a "connection refused"
        // that names a socket nobody is listening on.
        let p = kod_core_state::socket_path::default_socket_path();
        assert!(p.ends_with("kod.sock"), "got {p:?}");
    }
}

/// Coverage for the daemon's pure helpers and its small write
/// wrappers. None of these tests touches a real socket — the
/// framing is already covered by `serve_roundtrip`, and the
/// wrappers are thin enough that a `tokio::sync::mpsc` channel is
/// enough to observe their output.
#[cfg(test)]
mod coverage_serve_handlers {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::FileTypeExt as _;
    use tokio::sync::mpsc;

    async fn drain_one<F, Fut>(f: F) -> String
    where
        F: FnOnce(mpsc::Sender<String>) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let (tx, mut rx) = mpsc::channel::<String>(4);
        f(tx).await;
        rx.recv().await.expect("a response was written")
    }

    fn parse_line(line: &str) -> Value {
        serde_json::from_str(line.trim_end()).expect("response is valid JSON")
    }

    // ---- string_param ---------------------------------------------------

    #[test]
    fn string_param_returns_the_value_when_present() {
        let v = serde_json::json!({"input": "hello", "other": 1});
        assert_eq!(string_param(&v, "input"), "hello");
    }

    #[test]
    fn string_param_missing_key_is_empty_string() {
        let v = serde_json::json!({"input": "hello"});
        assert_eq!(string_param(&v, "absent"), "");
    }

    #[test]
    fn string_param_wrong_type_is_empty_string() {
        // A client that sent `{"input": 42}` gets "" rather than an
        // error; the engine then sees an empty prompt and reports
        // its own error, which is a better message than
        // "expected string, found number".
        let v = serde_json::json!({"input": 42});
        assert_eq!(string_param(&v, "input"), "");
        let v = serde_json::json!({"input": null});
        assert_eq!(string_param(&v, "input"), "");
        let v = serde_json::json!({"input": ["a"]});
        assert_eq!(string_param(&v, "input"), "");
    }

    #[test]
    fn string_param_on_null_params_is_empty() {
        assert_eq!(string_param(&Value::Null, "input"), "");
    }

    // ---- write_chunk ----------------------------------------------------

    #[tokio::test]
    async fn write_chunk_produces_a_chunk_frame() {
        let line = drain_one(|tx| async move {
            write_chunk(&tx, "r1", "hi").await.unwrap();
        })
        .await;
        let v = parse_line(&line);
        assert_eq!(v["id"], "r1");
        assert_eq!(v["type"], "chunk");
        assert_eq!(v["data"], "hi");
        assert!(line.ends_with('\n'), "line is newline-terminated");
    }

    #[tokio::test]
    async fn write_chunk_with_an_empty_string_is_still_a_frame() {
        let line = drain_one(|tx| async move {
            write_chunk(&tx, "r1", "").await.unwrap();
        })
        .await;
        let v = parse_line(&line);
        assert_eq!(v["type"], "chunk");
        assert_eq!(v["data"], "");
    }

    #[tokio::test]
    async fn write_chunk_with_unicode_round_trips() {
        let line = drain_one(|tx| async move {
            write_chunk(&tx, "r1", "café 🚀").await.unwrap();
        })
        .await;
        let v = parse_line(&line);
        assert_eq!(v["data"], "café 🚀");
    }

    // ---- write_error ----------------------------------------------------

    #[tokio::test]
    async fn write_error_carries_a_message_object() {
        let line = drain_one(|tx| async move {
            write_error(&tx, "r2", "boom").await.unwrap();
        })
        .await;
        let v = parse_line(&line);
        assert_eq!(v["id"], "r2");
        assert_eq!(v["type"], "error");
        assert_eq!(v["data"]["message"], "boom");
    }

    // ---- write_ack ------------------------------------------------------

    #[tokio::test]
    async fn write_ack_omits_data() {
        let line = drain_one(|tx| async move {
            write_ack(&tx, "r3").await.unwrap();
        })
        .await;
        let v = parse_line(&line);
        assert_eq!(v["id"], "r3");
        assert_eq!(v["type"], "done");
        assert!(v.get("data").is_none(), "ack must omit `data`, got: {line}");
    }

    // ---- write_ok -------------------------------------------------------

    #[tokio::test]
    async fn write_ok_carries_an_object_payload() {
        let line = drain_one(|tx| async move {
            write_ok(&tx, "r4", serde_json::json!({"delivered": true}))
                .await
                .unwrap();
        })
        .await;
        let v = parse_line(&line);
        assert_eq!(v["id"], "r4");
        assert_eq!(v["type"], "done");
        assert_eq!(v["data"]["delivered"], true);
    }

    #[tokio::test]
    async fn write_ok_with_an_array_payload_is_allowed() {
        let line = drain_one(|tx| async move {
            write_ok(&tx, "r5", serde_json::json!({"models": ["a", "b"]}))
                .await
                .unwrap();
        })
        .await;
        let v = parse_line(&line);
        assert_eq!(v["data"]["models"][0], "a");
        assert_eq!(v["data"]["models"][1], "b");
    }

    // ---- send_response --------------------------------------------------

    #[tokio::test]
    async fn send_response_returns_an_error_when_the_channel_is_closed() {
        // A writer task that has already exited leaves the sender
        // with no receiver; the write wrappers must propagate the
        // error rather than panic, so the read loop can decide
        // whether to keep going.
        let (tx, rx) = mpsc::channel::<String>(1);
        drop(rx);
        let r = Response {
            id: "r6",
            kind: "done",
            data: None,
        };
        assert!(
            send_response(&tx, &r).await.is_err(),
            "a closed channel must surface as an error",
        );
    }

    // ---- prepare_socket -------------------------------------------------

    // Unix-only: a Unix socket is a filesystem entry and needs its
    // parent directory. On Windows the path is a display-only mapping
    // to a named pipe, so there is no directory to create.
    #[cfg(unix)]
    #[tokio::test]
    async fn transport_bind_creates_the_parent_directory() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sock = tmp.path().join("nested").join("kod.sock");
        assert!(!sock.parent().unwrap().exists());
        let _l = crate::transport::bind(&sock).await.unwrap();
        assert!(
            sock.parent().unwrap().is_dir(),
            "transport::bind must create the parent directory",
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn transport_bind_creates_the_socket_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sock = tmp.path().join("kod.sock");
        let _l = crate::transport::bind(&sock).await.unwrap();
        // `bind_listener` returns a bound listener, so the socket
        // path exists and is a socket.
        let md = std::fs::metadata(&sock).expect("socket file exists");
        assert!(
            md.file_type().is_socket(),
            "transport::bind must leave a socket at the path",
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn transport_bind_replaces_a_stale_socket_file() {
        // A crashed daemon leaves an inode on disk that is not a
        // listening socket. `bind_listener` must clear it so the
        // next bind succeeds, rather than returning "already
        // listening" for a server that no longer exists.
        let tmp = tempfile::TempDir::new().unwrap();
        let sock = tmp.path().join("kod.sock");
        std::fs::write(&sock, b"stale").unwrap();
        assert!(sock.exists());
        let _l = crate::transport::bind(&sock).await.unwrap();
        // The path now holds a socket, not the stale text file.
        let md = std::fs::metadata(&sock).expect("path exists after bind");
        assert!(
            md.file_type().is_socket(),
            "transport::bind must replace the stale file with a socket",
        );
    }

    #[tokio::test]
    async fn prepare_socket_refuses_when_a_live_listener_exists() {
        // A real listening socket is the one case where the
        // daemon must refuse to start — a second `kod serve` would
        // silently steal connections from the first.
        let tmp = tempfile::TempDir::new().unwrap();
        let sock = tmp.path().join("kod.sock");
        let _listener = crate::transport::bind(&sock)
            .await
            .expect("bind test listener");
        let err = crate::transport::bind(&sock).await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("another kod server") || msg.contains("listening"),
            "expected an 'already listening' error, got: {msg}",
        );
    }

    // ---- set_socket_perms -----------------------------------------------

    #[cfg(unix)]
    #[test]
    fn set_socket_perms_creates_a_0600_file() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("perm_test");
        std::fs::write(&f, b"").unwrap();
        set_socket_perms(&f).unwrap();
        let mode = std::fs::metadata(&f).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "socket file must be 0600, got {mode:o}");
    }
}

#[cfg(test)]
mod version_tests {
    use super::*;

    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn version_range_constants_are_ordered() {
        assert!(MIN_PROTOCOL_VERSION <= PROTOCOL_VERSION);
        assert!(PROTOCOL_VERSION <= MAX_PROTOCOL_VERSION);
    }

    #[test]
    fn a_missing_version_parses_as_none() {
        // An older client predates the field; every v1 method works
        // without it, so the gate lets it through.
        let req: Request = serde_json::from_str(r#"{"id":"1","method":"list_models"}"#).unwrap();
        assert!(req.v.is_none());
    }

    #[test]
    fn an_out_of_range_version_is_outside_the_supported_range() {
        let req: Request =
            serde_json::from_str(r#"{"v":9,"id":"1","method":"list_models"}"#).unwrap();
        let v = req.v.unwrap();
        assert!(
            !(MIN_PROTOCOL_VERSION..=MAX_PROTOCOL_VERSION).contains(&v),
            "v={v} should be outside the server range",
        );
    }

    #[test]
    fn the_hello_overlap_is_the_intersection() {
        // Pure arithmetic over the same range the hello arm uses:
        // a client speaking 1..=3 against a server speaking 1..=1
        // overlaps at 1.
        let (client_min, client_max) = (1u8, 3u8);
        let lo = client_min.max(MIN_PROTOCOL_VERSION);
        let hi = client_max.min(MAX_PROTOCOL_VERSION);
        assert!(lo <= hi);
        assert_eq!(hi, 1);
    }

    #[test]
    fn a_disjoint_range_has_no_overlap() {
        let (client_min, client_max) = (5u8, 7u8);
        let lo = client_min.max(MIN_PROTOCOL_VERSION);
        let hi = client_max.min(MAX_PROTOCOL_VERSION);
        assert!(lo > hi, "5..=7 against 1..=1 must not overlap");
    }

    #[test]
    fn protocol_methods_are_sorted_and_unique() {
        // Sorted so the doc table and a client's generated list are
        // stable across builds; unique so a typo'd duplicate does not
        // hide a missing method.
        let mut sorted = PROTOCOL_METHODS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), PROTOCOL_METHODS.len(), "duplicates present");
        assert_eq!(sorted.as_slice(), PROTOCOL_METHODS, "must be sorted");
    }

    #[test]
    fn protocol_methods_are_well_formed() {
        for m in PROTOCOL_METHODS {
            assert!(!m.is_empty());
            assert!(
                m.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "method {m:?} is not snake_case",
            );
        }
    }

    #[test]
    fn client_min_clamps_large_values() {
        // T3-H10: `u64 -> u8` casts used to truncate. A client sending
        // `"min": 256` would silently land at 0 and fail the range
        // check with an opaque error.
        use serde_json::json;
        let mut params = serde_json::Map::new();
        params.insert("min".to_string(), json!(256u64));
        params.insert("max".to_string(), json!(1024u64));
        let v = serde_json::Value::Object(params);
        // The negotiation handler clamps; we call the same helper shape.
        let client_min = v
            .get("min")
            .and_then(|x| x.as_u64())
            .map(|n| n.min(u8::MAX as u64) as u8)
            .unwrap_or(1);
        let client_max = v
            .get("max")
            .and_then(|x| x.as_u64())
            .map(|n| n.min(u8::MAX as u64) as u8)
            .unwrap_or(1);
        assert_eq!(client_min, u8::MAX);
        assert_eq!(client_max, u8::MAX);
    }
}

#[cfg(test)]
mod panic_guard_tests {
    //! Pins the contract of `spawn_panic_guarded`: a panic in the
    //! spawned future must produce an error frame for the request id.
    //!
    //! Two layers:
    //!
    //! - `panicking_task_produces_an_error_frame` pins the helper in
    //!   isolation. It is the smallest possible test of the fix and
    //!   fails if `spawn_panic_guarded` is reverted to a bare spawn.
    //!
    //! - `process_request_with_panicking_provider_gets_error_frame`
    //!   drives the full `serve_io` path: a `process` request routed
    //!   to a provider that panics in `generate_with_tools`, through
    //!   the real dispatch loop. This is the E2E the helper test's
    //!   earlier honest-scope caveat named as a follow-up; the
    //!   `serve_io` extraction it needed landed in b38b7c2.
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn panicking_task_produces_an_error_frame() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(8);
        spawn_panic_guarded(
            async { panic!("simulated task panic") },
            tx,
            "req-1".to_string(),
        );
        let got = tokio::time::timeout(Duration::from_secs(1), rx.recv()).await;
        let line = match got {
            Ok(Some(line)) => line,
            Ok(None) => panic!(
                "channel closed with no error frame; the caller would wait on its request id"
            ),
            Err(_) => panic!("no error frame within 1s; the caller would wait on its request id"),
        };
        assert!(line.contains("\"error\""), "not an error frame: {line}");
        assert!(
            line.contains("req-1"),
            "error frame is not for req-1: {line}"
        );
    }

    // ---------------------------------------------------------------------
    // End-to-end: a panicking provider must not wedge the caller.
    // ---------------------------------------------------------------------

    use async_trait::async_trait;
    use futures::Stream;
    use kod_core::KodEngine;
    use kod_core::RouterConfig;
    use kod_provider::{
        GenerationOptions, GenerationResponse, LlmProvider, ModelRef, ProviderCapabilities,
        ProviderRegistry, StreamChunk,
    };
    use kod_types::ToolDefinition;
    use std::pin::Pin;
    use std::sync::Arc;

    /// A provider that panics on `generate_with_tools`. The panic
    /// propagates into `Engine::process_for`, which is wrapped by
    /// `spawn_panic_guarded` in the `process` dispatch arm — the
    /// real code path the fix guards.
    struct PanickingProvider;

    #[async_trait]
    impl LlmProvider for PanickingProvider {
        fn name(&self) -> &str {
            "panicking"
        }
        async fn list_models(&self) -> kod_error::Result<Vec<kod_provider::ModelInfo>> {
            Ok(vec![])
        }
        async fn generate(&self, _p: &str, _o: &GenerationOptions) -> kod_error::Result<String> {
            panic!("simulated provider panic in generate")
        }
        async fn generate_with_tools(
            &self,
            _prompt: &str,
            _tools: &[ToolDefinition],
            _opts: &GenerationOptions,
        ) -> kod_error::Result<GenerationResponse> {
            panic!("simulated provider panic in generate_with_tools")
        }
        fn stream(
            &self,
            _prompt: &str,
            _opts: &GenerationOptions,
        ) -> Pin<Box<dyn Stream<Item = kod_error::Result<StreamChunk>> + Send + '_>> {
            Box::pin(futures::stream::empty())
        }
    }

    /// Build an engine with `PanickingProvider` installed and started.
    async fn engine_with_panicking_provider(temp: &tempfile::TempDir) -> Arc<KodEngine> {
        let cfg = RouterConfig {
            embedder: None,
            skill_threshold: 0.3,
            working_dir: temp.path().to_path_buf(),
            enable_memory: false,
            max_skills_per_query: 3,
            context_window: 8192,
            short_term_capacity: 100,
        };
        let engine = Arc::new(KodEngine::new(cfg, temp.path().join("serve.redb")).unwrap());

        let provider: Arc<dyn LlmProvider> = Arc::new(PanickingProvider);
        let mut registry = ProviderRegistry::new();
        registry.insert(
            "panicking",
            provider,
            ProviderCapabilities::conservative(),
            "panic-model",
        );
        let mut routing = kod_config::RoutingConfig::default();
        routing
            .by_task
            .insert("Simple".to_string(), "panicking".to_string());
        engine
            .set_registry(
                Arc::new(registry),
                ModelRef::new("panicking", "panic-model"),
                Some(routing),
            )
            .await;
        engine.start().await.unwrap();
        engine
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn process_request_with_panicking_provider_gets_error_frame() {
        let temp = tempfile::TempDir::new().unwrap();
        let engine = engine_with_panicking_provider(&temp).await;

        let (client_w, server_r) = tokio::io::duplex(64 * 1024);
        let (server_w, client_r) = tokio::io::duplex(64 * 1024);
        let shutdown = Arc::new(tokio::sync::Notify::new());
        let shutdown_for_serve = shutdown.clone();

        let serve_handle =
            tokio::spawn(
                async move { serve_io(engine, server_r, server_w, shutdown_for_serve).await },
            );

        let req = serde_json::json!({
            "id": "r1",
            "method": "process",
            "params": {"input": "hello", "transcript_key": "e2e"}
        });
        let mut line = serde_json::to_string(&req).unwrap();
        line.push('\n');
        {
            use tokio::io::AsyncWriteExt;
            let mut cw = client_w;
            cw.write_all(line.as_bytes()).await.unwrap();
            cw.flush().await.unwrap();
        }

        use tokio::io::{AsyncBufReadExt, BufReader};
        let mut reader = BufReader::new(client_r);
        let mut buf = String::new();
        let read = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            reader.read_line(&mut buf),
        )
        .await
        .expect("no response frame within 5s; the guard did not send one")
        .expect("read failed");
        assert!(
            read > 0,
            "EOF on client side; the guard did not send a frame"
        );

        let frame: serde_json::Value = serde_json::from_str(buf.trim_end()).unwrap();
        assert_eq!(
            frame.get("id").and_then(|v| v.as_str()),
            Some("r1"),
            "error frame is not for r1: {frame}",
        );
        assert_eq!(
            frame.get("type").and_then(|v| v.as_str()),
            Some("error"),
            "not an error frame: {frame}",
        );

        shutdown.notify_one();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), serve_handle).await;
    }

    // ---------------------------------------------------------------------
    // Streaming variant: a panicking provider on `process_streaming`.
    // ---------------------------------------------------------------------
    //
    // `run_streaming` already wraps `process_streaming_for` in an
    // inner `tokio::spawn` and converts `JoinError` to
    // `KodError::Internal`, so the streaming path was not wedged by
    // a panicking engine even before `spawn_panic_guarded` landed.
    // This test pins that fact: it passes with the helper reverted
    // to a bare spawn (see the positive control in the bug-hunt
    // followup). `spawn_panic_guarded` at the streaming site is
    // defense-in-depth (a panic in `run_streaming`'s own body,
    // rather than the engine's), not the fix for this bug.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn streaming_request_with_panicking_provider_gets_error_frame() {
        let temp = tempfile::TempDir::new().unwrap();
        let engine = engine_with_panicking_provider(&temp).await;

        let (client_w, server_r) = tokio::io::duplex(64 * 1024);
        let (server_w, client_r) = tokio::io::duplex(64 * 1024);
        let shutdown = Arc::new(tokio::sync::Notify::new());
        let shutdown_for_serve = shutdown.clone();

        let serve_handle =
            tokio::spawn(
                async move { serve_io(engine, server_r, server_w, shutdown_for_serve).await },
            );

        let req = serde_json::json!({
            "id": "s1",
            "method": "process_streaming",
            "params": {"input": "hello", "transcript_key": "stream-e2e"}
        });
        let mut line = serde_json::to_string(&req).unwrap();
        line.push('\n');
        {
            use tokio::io::AsyncWriteExt;
            let mut cw = client_w;
            cw.write_all(line.as_bytes()).await.unwrap();
            cw.flush().await.unwrap();
        }

        use tokio::io::{AsyncBufReadExt, BufReader};
        let mut reader = BufReader::new(client_r);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            assert!(!remaining.is_zero(), "no error frame within 5s");
            let mut buf = String::new();
            let read = tokio::time::timeout(remaining, reader.read_line(&mut buf))
                .await
                .expect("read timeout; no error frame arrived")
                .expect("read failed");
            assert!(read > 0, "EOF before an error frame arrived");
            let frame: serde_json::Value = serde_json::from_str(buf.trim_end()).unwrap();
            match frame.get("type").and_then(|v| v.as_str()) {
                Some("error") => {
                    assert_eq!(
                        frame.get("id").and_then(|v| v.as_str()),
                        Some("s1"),
                        "error frame is not for s1: {frame}",
                    );
                    shutdown.notify_one();
                    let _ =
                        tokio::time::timeout(std::time::Duration::from_secs(5), serve_handle).await;
                    return;
                }
                Some("done") => panic!(
                    "streaming request reported done without an error; \
                     the panic was swallowed: {frame}"
                ),
                _ => {
                    // Streaming may emit chunks before the error;
                    // keep reading.
                    continue;
                }
            }
        }
    }
}

#[cfg(test)]
mod capped_lines_proptests {
    //! Properties for `CappedLines::next_line`.
    //!
    //! `CappedLines` reads a byte stream one `\n`-terminated line
    //! at a time, capped by a caller-supplied byte budget. It is
    //! the request-line reader for the ACP server; the 1 MiB cap
    //! is a hostile-peer guard, not a per-request limit a
    //! well-behaved client hits.
    //!
    //! **A real off-by-one.** The cap is enforced as
    //! `buf.len() + take > cap`, where `take` includes the `\n`
    //! terminator if one is present. That means:
    //!
    //! * `content + "\n"` with `content.len() == cap - 1`:
    //!   `take = cap`, `0 + cap > cap` is false -> accepts.
    //! * `content + "\n"` with `content.len() == cap`:
    //!   `take = cap + 1`, `cap + 1 > cap` -> **errors**.
    //! * `content` at EOF (no terminator) with
    //!   `content.len() == cap`: `take = cap`,
    //!   `0 + cap > cap` is false -> accepts.
    //!
    //! Same content, two outcomes, based on whether a terminator
    //! follows. The cap's unit is ambiguous between "line including
    //! terminator" and "content excluding terminator"; the code
    //! measures one or the other depending on the read path.
    //!
    //! `prop_terminator_does_not_change_admissibility` fails on the
    //! pre-fix code for `content.len() == cap`; the fix (below,
    //! same commit) makes the cap a *content* budget by subtracting
    //! the terminator from `take`.

    use super::*;
    use proptest::prelude::*;
    use std::io::Cursor;
    use tokio::io::BufReader;

    /// Drive `next_line` on a fixed byte slice. The reader is a
    /// `BufReader` over a `Cursor`, so `fill_buf` returns chunks
    /// without any real I/O.
    fn one_line(bytes: Vec<u8>, cap: usize) -> Result<Option<String>> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .build()
            .expect("single-thread rt");
        rt.block_on(async {
            let mut reader = BufReader::new(Cursor::new(bytes));
            let mut lines = CappedLines::new(&mut reader, cap);
            lines.next_line().await
        })
    }

    fn safe_content(max_len: usize) -> impl Strategy<Value = String> {
        prop::string::string_regex(&format!("[a-zA-Z0-9]{{0,{max_len}}}")).expect("regex compiles")
    }

    proptest! {
        /// A well-formed line with `\n` terminator yields its
        /// content, minus the terminator.
        #[test]
        fn prop_lf_stripped(content in safe_content(40)) {
            let mut bytes = content.clone().into_bytes();
            bytes.push(b'\n');
            let result = one_line(bytes, 1000).expect("ok");
            prop_assert_eq!(result, Some(content));
        }

        /// A well-formed line with `\r\n` terminator yields its
        /// content, minus both.
        #[test]
        fn prop_crlf_stripped(content in safe_content(40)) {
            let mut bytes = content.clone().into_bytes();
            bytes.extend_from_slice(b"\r\n");
            let result = one_line(bytes, 1000).expect("ok");
            prop_assert_eq!(result, Some(content));
        }

        /// EOF with no terminator yields the content. Excludes
        /// empty input: `next_line` returns `None` for that case,
        /// pinned separately by `prop_empty_returns_none`.
        #[test]
        fn prop_eof_no_terminator_returns_content(content in "[a-zA-Z0-9]{1,40}") {
            let bytes = content.clone().into_bytes();
            let result = one_line(bytes, 1000).expect("ok");
            prop_assert_eq!(result, Some(content));
        }

        /// Empty input returns `None`.
        #[test]
        fn prop_empty_returns_none(_dummy in 0u8..1) {
            let result = one_line(Vec::new(), 1000).expect("ok");
            prop_assert_eq!(result, None);
        }

        /// A bare `\n` returns `Some("")`.
        #[test]
        fn prop_bare_newline_returns_empty(_dummy in 0u8..1) {
            let result = one_line(b"\n".to_vec(), 1000).expect("ok");
            prop_assert_eq!(result, Some(String::new()));
        }

        /// **The bug.** `content + "\n"` and `content` at EOF must
        /// have the same admissibility at the cap boundary. Pre-fix
        /// the former errors and the latter accepts when
        /// `content.len() == cap`.
        #[test]
        fn prop_terminator_does_not_change_admissibility(
            cap in 1usize..=20usize,
        ) {
            let content: String = "x".repeat(cap);

            let with_lf = {
                let mut b = content.clone().into_bytes();
                b.push(b'\n');
                one_line(b, cap)
            };
            let at_eof = one_line(content.clone().into_bytes(), cap);

            prop_assert_eq!(
                with_lf.is_ok(),
                at_eof.is_ok(),
                "cap={}: with '\\n' is_ok={}, at EOF is_ok={}",
                cap,
                with_lf.is_ok(),
                at_eof.is_ok(),
            );
        }
    }

    /// Control: content of `cap - 1` bytes plus `\n` is admitted.
    /// The current code already accepts this; the control pins the
    /// lower edge of the boundary.
    #[test]
    fn control_cap_minus_one_with_lf_accepted() {
        let content = "x".repeat(10);
        let mut bytes = content.clone().into_bytes();
        bytes.push(b'\n');
        let result = one_line(bytes, 11);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), Some(content));
    }

    /// Control: content of `cap + 1` bytes plus `\n` is rejected.
    /// The current code rejects this; the control pins the upper
    /// edge of the boundary.
    #[test]
    fn control_over_cap_rejected() {
        let content = "x".repeat(12);
        let mut bytes = content.into_bytes();
        bytes.push(b'\n');
        let result = one_line(bytes, 10);
        assert!(result.is_err(), "12 content + 1 terminator > cap 10");
    }

    /// Control: no panic on invalid UTF-8. The reader uses
    /// `from_utf8_lossy`, so an invalid byte becomes U+FFFD, not an
    /// error. Pinned so a future strict-UTF-8 change is a
    /// decision.
    #[test]
    fn control_invalid_utf8_is_lossy() {
        let mut bytes = vec![0xFFu8, 0xFE];
        bytes.push(b'\n');
        let result = one_line(bytes, 10).expect("no error on invalid utf8");
        assert!(result.is_some());
        // Both bytes become replacement characters; the content is
        // whatever lossy decoding produces, which is not "FF FE".
        let s = result.unwrap();
        assert!(!s.is_empty());
    }
}
