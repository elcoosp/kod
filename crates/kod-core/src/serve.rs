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
        KodError::Internal(format!(
            "could not bind {}: {e}",
            socket_path.display()
        ))
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
    let mut line = serde_json::to_string(&req)
        .map_err(|e| KodError::Internal(e.to_string()))?;
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
    let cred = stream.peer_cred().map_err(|e| {
        KodError::Internal(format!("could not read peer credentials: {e}"))
    })?;
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
async fn handle_connection(
    stream: UnixStream,
    engine: Arc<KodEngine>,
    shutdown: Arc<Notify>,
) -> Result<()> {
    check_peer_uid(&stream)?;
    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();

    while let Some(line) = lines.next_line().await.map_err(KodError::Io)? {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let req: Request = match serde_json::from_str(trimmed) {
            Ok(r) => r,
            Err(e) => {
                write_error(&mut write_half, "", &format!("bad request: {e}")).await?;
                continue;
            }
        };

        match req.method.as_str() {
            "process" => {
                let input = string_param(&req.params, "input");
                let key = string_param(&req.params, "transcript_key");
                match engine.process_for(&key, &input).await {
                    Ok(resp) => write_done(&mut write_half, &req.id, &resp).await?,
                    Err(e) => write_error(&mut write_half, &req.id, &e.to_string()).await?,
                }
            }
            "process_streaming" => {
                let input = string_param(&req.params, "input");
                let key = string_param(&req.params, "transcript_key");
                let (chunk_tx, mut chunk_rx) =
                    tokio::sync::mpsc::channel::<String>(64);
                let engine_task = tokio::spawn({
                    let engine = engine.clone();
                    async move {
                        engine.process_streaming_for(&key, &input, &chunk_tx).await
                    }
                });
                while let Some(chunk) = chunk_rx.recv().await {
                    write_chunk(&mut write_half, &req.id, &chunk).await?;
                }
                let outcome = engine_task
                    .await
                    .map_err(|e| KodError::Internal(format!("engine task panicked: {e}")))?;
                match outcome {
                    Ok(resp) => write_done(&mut write_half, &req.id, &resp).await?,
                    Err(e) => write_error(&mut write_half, &req.id, &e.to_string()).await?,
                }
            }
            "steer" => {
                let note = string_param(&req.params, "note");
                let key = string_param(&req.params, "transcript_key");
                engine.steer_for(&key, &note).await;
                write_ack(&mut write_half, &req.id).await?;
            }
            "swarm" => {
                let goal = string_param(&req.params, "goal");
                if goal.trim().is_empty() {
                    write_error(&mut write_half, &req.id, "swarm: 'goal' is required")
                        .await?;
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

                let runner = match crate::swarm_runner::SwarmRunner::new(
                    engine.clone(),
                    max_agents,
                    merge,
                )
                .await
                {
                    Ok(r) => r,
                    Err(e) => {
                        write_error(&mut write_half, &req.id, &e.to_string()).await?;
                        continue;
                    }
                };

                let (evt_tx, mut evt_rx) =
                    tokio::sync::mpsc::channel::<crate::swarm_runner::SwarmEvent>(256);
                let goal_owned = goal.clone();
                let run_handle =
                    tokio::spawn(async move { runner.run(&goal_owned, &evt_tx).await });

                // The event type derives Serialize, so the wire
                // shape is produced by serde, not a hand-rolled
                // match arm per variant.
                while let Some(evt) = evt_rx.recv().await {
                    let data = serde_json::to_value(&evt)
                        .unwrap_or(serde_json::Value::Null);
                    write_line(
                        &mut write_half,
                        &Response {
                            id: &req.id,
                            kind: "swarm_event",
                            data: Some(data),
                        },
                    )
                    .await?;
                }

                let outcome = run_handle.await.map_err(|e| {
                    KodError::Internal(format!("swarm task panicked: {e}"))
                })?;
                match outcome {
                    Ok(resp) => {
                        let data = serde_json::json!({
                            "merged": resp.merged,
                            "merged_by_model": resp.merged_by_model,
                            "conflicts": resp.conflicts.iter().map(|c| {
                                serde_json::json!({
                                    "file": c.file,
                                    "agents": c.agents,
                                })
                            }).collect::<Vec<_>>(),
                        });
                        write_ok(&mut write_half, &req.id, data).await?;
                    }
                    Err(e) => {
                        write_error(&mut write_half, &req.id, &e.to_string()).await?;
                    }
                }
            }
            "cancel" => {
                let key = string_param(&req.params, "transcript_key");
                engine.request_cancel_for(&key);
                write_ack(&mut write_half, &req.id).await?;
            }
            "shutdown" => {
                write_ack(&mut write_half, &req.id).await?;
                shutdown.notify_one();
                return Ok(());
            }
            "list_models" => match engine.list_models().await {
                Ok(models) => {
                    let data = serde_json::json!({ "models": models });
                    write_ok(&mut write_half, &req.id, data).await?
                }
                Err(e) => write_error(&mut write_half, &req.id, &e.to_string()).await?,
            },
            "set_model" => {
                let endpoint = string_param(&req.params, "endpoint");
                let model = string_param(&req.params, "model");
                engine
                    .set_current_model(ModelRef::new(endpoint, model))
                    .await;
                write_ack(&mut write_half, &req.id).await?;
            }
            other => {
                write_error(
                    &mut write_half,
                    &req.id,
                    &format!("unknown method: {other}"),
                )
                .await?;
            }
        }
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

async fn write_line<W: AsyncWriteExt + Unpin>(
    w: &mut W,
    response: &Response<'_>,
) -> Result<()> {
    let mut s = serde_json::to_string(response)
        .map_err(|e| KodError::Serialization(e.to_string()))?;
    s.push('\n');
    w.write_all(s.as_bytes()).await.map_err(KodError::Io)?;
    w.flush().await.map_err(KodError::Io)?;
    Ok(())
}

async fn write_chunk<W: AsyncWriteExt + Unpin>(
    w: &mut W,
    id: &str,
    chunk: &str,
) -> Result<()> {
    write_line(
        w,
        &Response {
            id,
            kind: "chunk",
            data: Some(Value::String(chunk.to_string())),
        },
    )
    .await
}

async fn write_done<W: AsyncWriteExt + Unpin>(
    w: &mut W,
    id: &str,
    resp: &crate::router::TaskResponse,
) -> Result<()> {
    let data = serde_json::to_value(resp)
        .unwrap_or(Value::Null);
    write_line(
        w,
        &Response {
            id,
            kind: "done",
            data: Some(data),
        },
    )
    .await
}

async fn write_error<W: AsyncWriteExt + Unpin>(
    w: &mut W,
    id: &str,
    message: &str,
) -> Result<()> {
    write_line(
        w,
        &Response {
            id,
            kind: "error",
            data: Some(serde_json::json!({ "message": message })),
        },
    )
    .await
}

async fn write_ack<W: AsyncWriteExt + Unpin>(w: &mut W, id: &str) -> Result<()> {
    write_line(
        w,
        &Response {
            id,
            kind: "done",
            data: None,
        },
    )
    .await
}

async fn write_ok<W: AsyncWriteExt + Unpin>(
    w: &mut W,
    id: &str,
    data: Value,
) -> Result<()> {
    write_line(
        w,
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
        let with: Request =
            serde_json::from_str(r#"{"v":1,"id":"a","method":"process"}"#).unwrap();
        assert_eq!(with.method, "process");
        let without: Request =
            serde_json::from_str(r#"{"id":"a","method":"process"}"#).unwrap();
        assert_eq!(without.method, "process");
    }
}
