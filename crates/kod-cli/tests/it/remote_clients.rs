//! `run_agent_remote` and `run_swarm_remote` end-to-end against a
//! one-shot `UnixListener`.
//!
//! Both are socket clients in the shape of `run_prompt_remote`: no
//! engine, no config load, just `transport::connect` and a JSON frame
//! exchange. The fake daemon below writes one frame of the test's
//! choosing and closes; the client's response loop is the code under
//! test.
//!
//! Covered per function: empty-goal guard, missing-socket guard, the
//! `done` frame's Ok return, and the `error` frame's Err return.
#![cfg(unix)]

use kod_cli::commands::chat::run_agent_remote;
use kod_cli::commands::swarm::run_swarm_remote;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

/// Bind a one-shot listener under `dir`, read one request line, write
/// `frames`, then close.
fn fake_daemon(
    dir: &std::path::Path,
    frames: &'static [u8],
) -> (std::path::PathBuf, tokio::task::JoinHandle<()>) {
    let sock = dir.join("daemon.sock");
    let listener = UnixListener::bind(&sock).expect("bind test socket");
    let handle = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept one client");
        let (read_half, mut write_half) = stream.into_split();
        let mut reader = BufReader::new(read_half);
        let mut request = String::new();
        reader
            .read_line(&mut request)
            .await
            .expect("read request line");
        write_half.write_all(frames).await.expect("write frames");
        write_half.flush().await.ok();
    });
    (sock, handle)
}

// ---- run_agent_remote ----------------------------------------------

#[tokio::test]
async fn agent_remote_empty_goal_is_config_error() {
    let err = run_agent_remote("name".to_string(), "   ".to_string(), None)
        .await
        .expect_err("an empty goal must be an error");
    assert!(
        err.to_string().contains("empty goal"),
        "unexpected error: {err}",
    );
}

#[tokio::test]
async fn agent_remote_nonexistent_socket_is_invalid_state() {
    let tmp = tempfile::TempDir::new().unwrap();
    let sock = tmp.path().join("absent.sock");
    let err = run_agent_remote("name".to_string(), "hello".to_string(), Some(sock))
        .await
        .expect_err("a nonexistent socket must be an error");
    let msg = err.to_string();
    assert!(
        msg.contains("no daemon listening"),
        "unexpected error: {msg}"
    );
    assert!(msg.contains("kod serve"), "missing fix hint: {msg}");
}

#[tokio::test]
async fn agent_remote_done_frame_is_ok() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (sock, server) = fake_daemon(
        tmp.path(),
        b"{\"v\":1,\"id\":\"agent-1\",\"type\":\"done\",\"data\":{\"text\":\"hi\"}}\n",
    );
    run_agent_remote("name".to_string(), "hello".to_string(), Some(sock))
        .await
        .expect("a done frame must end the agent with Ok");
    server.await.expect("fake daemon must not panic");
}

#[tokio::test]
async fn agent_remote_error_frame_is_err() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (sock, server) = fake_daemon(
        tmp.path(),
        b"{\"v\":1,\"id\":\"agent-1\",\"type\":\"error\",\"data\":{\"message\":\"boom\"}}\n",
    );
    run_agent_remote("name".to_string(), "hello".to_string(), Some(sock))
        .await
        .expect_err("an error frame must be an Err");
    server.await.expect("fake daemon must not panic");
}

// ---- run_swarm_remote ----------------------------------------------

#[tokio::test]
async fn swarm_remote_empty_goal_is_config_error() {
    let err = run_swarm_remote("   ".to_string(), None, false, None)
        .await
        .expect_err("an empty goal must be an error");
    assert!(
        err.to_string().contains("empty goal"),
        "unexpected error: {err}",
    );
}

#[tokio::test]
async fn swarm_remote_nonexistent_socket_is_invalid_state() {
    let tmp = tempfile::TempDir::new().unwrap();
    let sock = tmp.path().join("absent.sock");
    let err = run_swarm_remote("hello".to_string(), None, false, Some(sock))
        .await
        .expect_err("a nonexistent socket must be an error");
    let msg = err.to_string();
    assert!(
        msg.contains("no daemon listening"),
        "unexpected error: {msg}"
    );
    assert!(msg.contains("kod serve"), "missing fix hint: {msg}");
}

#[tokio::test]
async fn swarm_remote_done_frame_is_ok() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (sock, server) = fake_daemon(
        tmp.path(),
        b"{\"v\":1,\"id\":\"swarm-1\",\"type\":\"done\",\"data\":{\"merged\":\"synthesized\"}}\n",
    );
    run_swarm_remote("hello".to_string(), None, false, Some(sock))
        .await
        .expect("a done frame must end the swarm with Ok");
    server.await.expect("fake daemon must not panic");
}

#[tokio::test]
async fn swarm_remote_error_frame_is_err() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (sock, server) = fake_daemon(
        tmp.path(),
        b"{\"v\":1,\"id\":\"swarm-1\",\"type\":\"error\",\"data\":{\"message\":\"boom\"}}\n",
    );
    run_swarm_remote("hello".to_string(), None, false, Some(sock))
        .await
        .expect_err("an error frame must be an Err");
    server.await.expect("fake daemon must not panic");
}

/// A `swarm_event` frame of kind `decomposed` followed by a `done`
/// frame: `run_swarm_remote` prints the subtask list (via
/// `print_swarm_event`) and then returns `Ok`. The other event kinds
/// (`worktrees_merged`, `agent_retrying`, etc.) share the same dispatch
/// and are not exercised here — one variant is enough to prove the
/// `Some("swarm_event") => print_swarm_event` arm is reached.
#[tokio::test]
async fn swarm_remote_decomposed_event_is_ok() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (sock, server) = fake_daemon(
        tmp.path(),
        concat!(
            "{\"v\":1,\"id\":\"swarm-1\",\"type\":\"swarm_event\",",
            "\"data\":{\"kind\":\"decomposed\",\"subtasks\":[",
            "{\"name\":\"alpha\",\"description\":\"do a\"},",
            "{\"name\":\"beta\",\"description\":\"do b\"}",
            "]}}\n",
            "{\"v\":1,\"id\":\"swarm-1\",\"type\":\"done\",",
            "\"data\":{\"merged\":\"synthesized\"}}\n",
        )
        .as_bytes(),
    );
    run_swarm_remote("hello".to_string(), None, false, Some(sock))
        .await
        .expect("decomposed event then done must be Ok");
    server.await.expect("fake daemon must not panic");
}
