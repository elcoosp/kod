//! `run_chat` (the local, embedded REPL) end-to-end against a mock
//! endpoint, driven by a spawn + piped stdin.
//!
//! `run_chat` reads stdin with `stdin.lock().read_line()`, so it does
//! not need a PTY — a piped child process with "hi\nquit\n" on stdin
//! runs one turn against the mock, prints the reply, reads "quit", and
//! exits 0. The same config/binary shape as the wrapper tests:
//! `KOD_CONFIG_DIR` for the config, `HOME` for the DB the wrapper
//! opens via `db_path: None`, `KOD_SESSIONS_DIR` for the recorder.
//!
//! The child is spawned with `tokio::process::Command` so
//! `tokio::time::timeout` can kill a hang; nextest's 60s terminate
//! would also catch it, but a 20s bound keeps a broken round quick.
use crate::common::mock_llm::MockLlm;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

fn kod_bin() -> &'static str {
    env!("CARGO_BIN_EXE_kod")
}

/// Spawn `kod chat` with `input` on stdin, wait at most 20s, return
/// the output. A timeout is a test failure, not a hang.
async fn run_chat_with_input(cfg: &std::path::Path, input: &[u8]) -> std::process::Output {
    use tokio::process::Command;
    let mut child = Command::new(kod_bin())
        .arg("chat")
        .env("HOME", cfg)
        .env("KOD_CONFIG_DIR", cfg)
        .env("KOD_SESSIONS_DIR", cfg)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn kod chat");
    let mut stdin = child.stdin.take().expect("take stdin");
    stdin.write_all(input).await.expect("write stdin");
    drop(stdin);
    tokio::time::timeout(Duration::from_secs(20), child.wait_with_output())
        .await
        .expect("kod chat must exit within 20s of stdin EOF")
        .expect("wait for kod chat")
}

/// `hi\nquit\n`: one prompt ("hi" hits the mock), then "quit" exits.
/// Asserts (a) the mock was hit, (b) the child exited 0. The child's
/// stdout is where the reply lands; not asserted on because it goes
/// through the streaming printer with marker filtering, and pinning
/// that output is the TUI's job (`tui_e2e`), not this smoke test's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn run_chat_exits_after_quit_and_hits_the_mock() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mock = MockLlm::start("chat reply").await;
    mock.write_config(tmp.path(), "");

    let out = run_chat_with_input(tmp.path(), b"hi\nquit\n").await;
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "kod chat must exit 0 after 'quit', got {:?}\nstderr:\n{}",
        out.status.code(),
        stderr,
    );
    assert!(
        mock.hits() >= 1,
        "the 'hi' line must have produced one model call; zero hits \
         means the config fell back to a default endpoint",
    );
}

/// Stdin EOF with no prompts: `run_chat` exits cleanly without ever
/// calling the model. The `hits() == 0` assertion pins that the
/// pre-prompt setup (config, engine, policy) is what the child runs,
/// not a spuriously successful model call.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn run_chat_exits_on_eof_without_hitting_the_mock() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mock = MockLlm::start("should not be reached").await;
    mock.write_config(tmp.path(), "");

    let out = run_chat_with_input(tmp.path(), b"").await;
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "kod chat must exit 0 on stdin EOF, got {:?}\nstderr:\n{}",
        out.status.code(),
        stderr,
    );
    assert_eq!(
        mock.hits(),
        0,
        "empty stdin must exit before any model call",
    );
}
