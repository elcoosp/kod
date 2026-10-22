//! `run_chat_remote`'s pre-stdin guard.
//!
//! `run_chat_remote` is a REPL: it prints a prompt, reads stdin, and
//! connects per turn. The one piece testable without feeding stdin is
//! the `#[cfg(unix)]` guard that rejects a socket path which has no
//! listener — it returns before the loop is entered.
//!
//! Gated `#[cfg(unix)]` because the guard itself is unix-only: on
//! Windows the check is skipped and the function falls through to the
//! REPL, which would block on stdin under `cargo nextest`.
#[cfg(unix)]
#[tokio::test]
async fn chat_remote_nonexistent_socket_is_invalid_state() {
    let tmp = tempfile::TempDir::new().unwrap();
    let sock = tmp.path().join("absent.sock");
    let err = kod_cli::commands::chat::run_chat_remote(Some(sock))
        .await
        .expect_err("a nonexistent socket path must be an error");
    let msg = err.to_string();
    assert!(
        msg.contains("no daemon listening"),
        "unexpected error: {msg}",
    );
    assert!(
        msg.contains("kod serve"),
        "missing fix hint in: {msg}",
    );
}
