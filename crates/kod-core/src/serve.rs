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
//! | Method              | Params                                            |
//! |---------------------|---------------------------------------------------|
//! | `process`           | `{input, transcript_key?}`                        |
//! | `process_streaming` | `{input, transcript_key?}`                        |
//! | `steer`             | `{note, transcript_key?}`                         |
//! | `cancel`            | `{transcript_key?}`                               |
//! | `shutdown`          | `{}`                                              |
//! | `list_models`       | `{}`                                              |
//! | `set_model`         | `{endpoint, model}`                               |
//!
//! # Lifecycle
//!
//! One server per user. `serve` binds the socket and refuses to
//! start if the file already exists and a live process is on it —
//! a stale socket from a crashed daemon is cleaned first. On
//! `shutdown` (or `SIGINT`) the accept loop exits, the socket file
//! is removed, and the call returns.

use crate::engine::KodEngine;
use kod_error::{KodError, Result};
use kod_provider::ModelRef;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Notify;

/// Protocol version. Sent by the client in every request; the
/// server ignores a missing version (older clients) but reserves
/// the right to reject a higher one in a future release.
pub const PROTOCOL_VERSION: u8 = 1;

/// Default socket path for this user.
pub fn default_socket_path() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        let dir = PathBuf::from(dir);
        if dir.is_dir() {
            return dir.join("kod.sock");
        }
    }
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    home.join(".kod").join("run").join("kod.sock")
}

/// A request line.
#[derive(Debug, Deserialize)]
struct Request {
    #[serde(default)]
    #[allow(dead_code)]
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
    prepare_socket(&socket_path).await?;
    let listener = UnixListener::bind(&socket_path).map_err(|e| {
        KodError::Internal(format!("could not bind {}: {e}", socket_path.display()))
    })?;
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
                    Ok((stream, _)) => {
                        let engine = engine.clone();
                        let shutdown = shutdown.clone();
                        tokio::spawn(async move {
                            if let Err(e) =
                                handle_connection(stream, engine, shutdown).await
                            {
                                tracing::debug!(error = %e, "connection ended");
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
    let mut stream = UnixStream::connect(socket_path).await.map_err(|e| {
        KodError::Internal(format!(
            "could not connect to {}: {e}",
            socket_path.display()
        ))
    })?;
    let req = serde_json::json!({
        "v": PROTOCOL_VERSION,
        "id": "stop-1",
        "method": "shutdown",
        "params": {},
    });
    let mut line = serde_json::to_string(&req).map_err(|e| KodError::Internal(e.to_string()))?;
    line.push('\n');
    stream.write_all(line.as_bytes()).await?;
    stream.flush().await?;
    // Read until the daemon closes the connection or answers. The
    // socket being unlinked is the true signal, which the CLI's
    // `kod serve --stop` polls for separately.
    let mut reader = BufReader::new(stream);
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

/// Remove a stale socket file, or refuse to start if a live daemon
/// is already listening on it.
async fn prepare_socket(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(KodError::Io)?;
    }
    match UnixStream::connect(path).await {
        Ok(_) => Err(KodError::InvalidState(format!(
            "another kod server is already listening on {}",
            path.display()
        ))),
        Err(_) => {
            // Either no file, or a stale one from a crashed daemon.
            // Removing is safe: if a live server were there, connect
            // would have succeeded and we would not be here.
            let _ = std::fs::remove_file(path);
            Ok(())
        }
    }
}

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

/// Verify the peer's UID matches ours before reading a single byte.
///
/// Uses `tokio::net::UnixStream::peer_cred`, which abstracts
/// `SO_PEERCRED` on Linux and `getpeereid` on macOS/BSD — the same
/// syscalls a hand-rolled version would call through `libc`,
/// reached through tokio's stable API instead. This removes a
/// `libc` dependency from `kod-core` entirely.
///
/// The check is the security boundary for the socket. The
/// filesystem permissions (`0600`) are the user-visible signal a
/// `ls -la` shows, but a same-user process on a multi-user machine
/// is exactly what the UID check is for.
fn check_peer_uid(stream: &UnixStream) -> Result<()> {
    let cred = stream
        .peer_cred()
        .map_err(|e| KodError::Internal(format!("could not read peer credentials: {e}")))?;
    let our_uid = current_uid();
    if cred.uid() != our_uid {
        return Err(KodError::PermissionDenied {
            action: "connect".to_string(),
            reason: format!(
                "peer uid {} does not match server uid {}",
                cred.uid(),
                our_uid
            ),
        });
    }
    Ok(())
}

/// The current process's UID.
///
/// `std` does not expose this on stable. The C `getuid` symbol is
/// linked into every Rust binary on Unix by the standard library's
/// own C runtime linkage, so a direct `extern "C"` binding adds no
/// dependency.
#[cfg(unix)]
fn current_uid() -> u32 {
    // SAFETY: `getuid` has no preconditions and cannot fail.
    unsafe { getuid() }
}

#[cfg(unix)]
unsafe extern "C" {
    fn getuid() -> u32;
}

#[cfg(not(unix))]
fn current_uid() -> u32 {
    0
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
async fn handle_connection(
    stream: UnixStream,
    engine: Arc<KodEngine>,
    shutdown: Arc<Notify>,
) -> Result<()> {
    check_peer_uid(&stream)?;
    let (read_half, write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();

    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<String>(256);
    let writer_task = tokio::spawn(async move {
        let mut write_half = write_half;
        while let Some(line) = out_rx.recv().await {
            if write_half.write_all(line.as_bytes()).await.is_err() {
                break;
            }
            if write_half.flush().await.is_err() {
                break;
            }
        }
    });

    while let Some(line) = lines.next_line().await.map_err(KodError::Io)? {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let req: Request = match serde_json::from_str(trimmed) {
            Ok(r) => r,
            Err(e) => {
                let _ = send_response(
                    &out_tx,
                    &Response {
                        id: "",
                        kind: "error",
                        data: Some(serde_json::json!({"message": format!("bad request: {e}")})),
                    },
                )
                .await;
                continue;
            }
        };

        match req.method.as_str() {
            "process" => {
                let input = string_param(&req.params, "input");
                let key = string_param(&req.params, "transcript_key");
                match engine.process_for(&key, &input).await {
                    Ok(resp) => write_done(&out_tx, &req.id, &resp).await?,
                    Err(e) => write_error(&out_tx, &req.id, &e.to_string()).await?,
                }
            }
            "process_streaming" => {
                let engine = engine.clone();
                let out = out_tx.clone();
                let id = req.id.clone();
                let input = string_param(&req.params, "input");
                let key = string_param(&req.params, "transcript_key");
                tokio::spawn(async move {
                    if let Err(e) = run_streaming(&engine, &out, &id, &input, &key).await {
                        let _ = send_response(
                            &out,
                            &Response {
                                id: &id,
                                kind: "error",
                                data: Some(serde_json::json!({"message": e.to_string()})),
                            },
                        )
                        .await;
                    }
                });
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
                    Some("approve") => Some(crate::engine::ApprovalDecision::Approve),
                    Some("deny") => Some(crate::engine::ApprovalDecision::Deny),
                    Some("deny_always") => Some(crate::engine::ApprovalDecision::DenyAlways),
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
                tokio::spawn(async move {
                    if let Err(e) = run_swarm(&engine, &out, &id, &goal, max_agents, merge).await {
                        let _ = send_response(
                            &out,
                            &Response {
                                id: &id,
                                kind: "error",
                                data: Some(serde_json::json!({"message": e.to_string()})),
                            },
                        )
                        .await;
                    }
                });
            }
            "shutdown" => {
                write_ack(&out_tx, &req.id).await?;
                shutdown.notify_one();
                break;
            }
            "list_models" => match engine.list_models().await {
                Ok(models) => {
                    let data = serde_json::json!({ "models": models });
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
                write_error(&out_tx, &req.id, &format!("unknown method: {other}")).await?;
            }
        }
    }

    // Reader is done. Drop our own sender, then wait a bounded time
    // for the writer task to drain what spawned pumps have already
    // queued. Without the timeout a hung streaming task would keep
    // the connection handler alive forever.
    drop(out_tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), writer_task).await;
    Ok(())
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
    while let Some(chunk) = chunk_rx.recv().await {
        write_chunk(out, req_id, &chunk).await?;
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
    let runner = crate::swarm_runner::SwarmRunner::new(engine.clone(), max_agents, merge).await?;
    let (evt_tx, mut evt_rx) = tokio::sync::mpsc::channel::<crate::swarm_runner::SwarmEvent>(256);
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
    resp: &crate::router::TaskResponse,
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
        let p = default_socket_path();
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
        let p = default_socket_path();
        assert!(p.ends_with("kod.sock"), "got {p:?}");
    }
}
