//! `run_prompt_remote`'s argument guards.
//!
//! The function's main path connects to a Unix-socket daemon and
//! streams its reply; testing that needs a daemon fixture, which is a
//! separate round. The guards before the connect are testable with
//! no daemon at all — and they are the ones the `process::exit` sweep
//! touched (the daemon-error frame after the loop, which needs a
//! daemon, and the empty-prompt guard, which does not).
//!
//! `run_prompt_remote` is `pub`, so a test can call it. Every test
//! here fails at the guard *before* the socket connect, so no daemon
//! is required.
use kod_cli::commands::prompt::run_prompt_remote;

/// An empty prompt is rejected before any socket work.
#[tokio::test]
async fn empty_prompt_is_rejected_before_connecting() {
    let err = run_prompt_remote("   ".to_string(), None)
        .await
        .expect_err("an empty prompt must be an error");
    assert!(err.to_string().contains("empty"), "unexpected error: {err}",);
}

/// A whitespace-only prompt (tabs and newlines) is also empty after
/// `trim()`, and the guard fires before the connect.
#[tokio::test]
async fn whitespace_only_prompt_is_rejected() {
    let err = run_prompt_remote("\t\n  ".to_string(), None)
        .await
        .expect_err("a whitespace-only prompt must be an error");
    assert!(err.to_string().contains("empty"), "unexpected error: {err}",);
}
