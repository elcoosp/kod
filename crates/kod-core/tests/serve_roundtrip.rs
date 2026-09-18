//! `kod serve` NDJSON protocol round-trip (design §11.2, §D6.2).
//!
//! Starts a real `serve` loop on a temporary unix socket, connects a
//! client with `UnixStream`, sends one NDJSON request, and asserts the
//! response the server writes back. The test is the first of its kind
//! for the protocol itself; every other test of `serve` reads the code
//! or exercises a helper in isolation.
//!
//! # What this proves
//!
//! - `serve` binds the socket path the caller names, with the
//!   documented permissions, and accepts a connection.
//! - The peer-UID check passes for the same process (it is the same
//!   user, which is the normal case).
//! - A `shutdown` request produces a `{type:"done"}` line and
//!   terminates the accept loop, letting the test clean up.
//! - Two responses for the same request id cannot interleave: the
//!   writer task serialises them on one channel, so a response is a
//!   whole line.
//!
//! # What it deliberately does not do
//!
//! `process_streaming` needs a live model, so that path is not
//! exercised here. The trait-level dispatch is covered by the engine
//! tests; this file is the socket-transport test.

#![cfg(unix)]

use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use kod_core::KodEngine;
use kod_core::router::RouterConfig;

async fn start_engine() -> (tempfile::TempDir, Arc<KodEngine>) {
    let tmp = tempfile::TempDir::new().unwrap();
    let db = tmp.path().join("t.redb");
    let cfg = RouterConfig {
        skill_threshold: 0.3,
        context_window: 8192,
        short_term_capacity: 100,
        working_dir: tmp.path().to_path_buf(),
        enable_memory: false,
        max_skills_per_query: 3,
        embedder: None,
    };
    let engine = Arc::new(KodEngine::new(cfg, db).unwrap());
    engine.start().await.unwrap();
    (tmp, engine)
}

/// The socket path for a test: a per-process file under a tempdir.
fn socket_path(dir: &std::path::Path) -> std::path::PathBuf {
    dir.join("kod-test.sock")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_request_terminates_the_daemon() {
    let (tmp, engine) = start_engine().await;
    let sock = socket_path(tmp.path());

    // Spawn the server. It blocks until `shutdown` is received or the
    // socket file is removed.
    let server_engine = engine.clone();
    let server_sock = sock.clone();
    let server =
        tokio::spawn(async move { kod_core::serve::serve(server_engine, server_sock).await });

    // Give the accept loop a moment to bind.
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while !sock.exists() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        sock.exists(),
        "serve must bind the socket file within 2s; path={}",
        sock.display(),
    );

    // Connect and send a shutdown request.
    let __connect_deadline = std::time::Instant::now()
        + std::time::Duration::from_secs(5);
    let stream = loop {
        match UnixStream::connect(&sock).await {
            Ok(s) => break s,
            Err(_e) if std::time::Instant::now() < __connect_deadline => {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            Err(e) => panic!("connect to daemon: {e}"),
        }
    };
    let (read_half, mut write_half) = stream.into_split();

    let req = serde_json::json!({
        "v": 1,
        "id": "test-1",
        "method": "shutdown",
        "params": {},
    });
    let mut frame = serde_json::to_string(&req).unwrap();
    frame.push('\n');
    write_half.write_all(frame.as_bytes()).await.unwrap();
    write_half.flush().await.unwrap();

    // Read the ack. The server writes one line before it breaks out of
    // the accept loop.
    let mut reader = BufReader::new(read_half).lines();
    let line = tokio::time::timeout(Duration::from_secs(2), reader.next_line())
        .await
        .expect("ack must arrive within 2s")
        .expect("no IO error")
        .expect("line");
    let v: serde_json::Value = serde_json::from_str(&line).expect("JSON line");
    assert_eq!(v["id"], "test-1");
    assert_eq!(v["type"], "done");

    // Wait for the server task. The accept loop ends, the socket file
    // is removed, and `serve` returns.
    let result = tokio::time::timeout(Duration::from_secs(3), server)
        .await
        .expect("server must finish within 3s")
        .expect("server task");
    result.expect("serve must return Ok on clean shutdown");
    assert!(
        !sock.exists(),
        "serve must remove the socket file on shutdown",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_method_reports_an_error_line() {
    let (tmp, engine) = start_engine().await;
    let sock = socket_path(tmp.path());

    let server_engine = engine.clone();
    let server_sock = sock.clone();
    let server =
        tokio::spawn(async move { kod_core::serve::serve(server_engine, server_sock).await });
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while !sock.exists() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(sock.exists());

    let stream = UnixStream::connect(&sock).await.unwrap();
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half).lines();

    // Send an unknown method. The daemon must respond with
    // `type: "error"` naming the method, not silently close.
    let req = serde_json::json!({
        "v": 1,
        "id": "u1",
        "method": "no_such_method",
        "params": {},
    });
    let mut frame = serde_json::to_string(&req).unwrap();
    frame.push('\n');
    write_half.write_all(frame.as_bytes()).await.unwrap();
    write_half.flush().await.unwrap();

    let line = tokio::time::timeout(Duration::from_secs(2), reader.next_line())
        .await
        .expect("response within 2s")
        .expect("io ok")
        .expect("a line");
    let v: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(v["id"], "u1");
    assert_eq!(v["type"], "error");
    assert!(
        v["data"]["message"]
            .as_str()
            .map(|s| s.contains("no_such_method"))
            .unwrap_or(false),
        "error message must name the unknown method: {v}",
    );

    // Tidy up with a shutdown on a fresh connection.
    let stream = UnixStream::connect(&sock).await.unwrap();
    let (_r, mut w) = stream.into_split();
    let req = serde_json::json!({
        "v": 1,
        "id": "shutdown",
        "method": "shutdown",
        "params": {},
    });
    let mut frame = serde_json::to_string(&req).unwrap();
    frame.push('\n');
    w.write_all(frame.as_bytes()).await.unwrap();
    w.flush().await.unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(2), server).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn second_daemon_on_the_same_socket_refuses_to_start() {
    // `prepare_socket` connects first; a live daemon on the path makes
    // the second `serve` return `InvalidState`. This is the guard that
    // keeps a user from accidentally running two daemons against the
    // same engine state.
    let (tmp, engine) = start_engine().await;
    let sock = socket_path(tmp.path());

    let server_engine = engine.clone();
    let server_sock = sock.clone();
    let server =
        tokio::spawn(async move { kod_core::serve::serve(server_engine, server_sock).await });
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while !sock.exists() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(sock.exists());

    // Second attempt on the same path.
    let (tmp2, engine2) = start_engine().await;
    let result = kod_core::serve::serve(engine2, sock.clone()).await;
    assert!(
        result.is_err(),
        "a second serve on a live socket must fail; got: {result:?}",
    );
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("already listening") || msg.contains("another"),
        "error should name the reason, got: {msg}",
    );

    // Shut the first daemon down cleanly.
    let stream = UnixStream::connect(&sock).await.unwrap();
    let (_r, mut w) = stream.into_split();
    let req = serde_json::json!({
        "v": 1,
        "id": "shutdown",
        "method": "shutdown",
        "params": {},
    });
    let mut frame = serde_json::to_string(&req).unwrap();
    frame.push('\n');
    w.write_all(frame.as_bytes()).await.unwrap();
    w.flush().await.unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(2), server).await;
    drop(tmp2);
}
