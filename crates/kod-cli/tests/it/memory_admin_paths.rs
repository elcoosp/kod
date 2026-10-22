//! Path-injected functions in `memory.rs` and `admin.rs`.
//!
//! `run_replay` and `run_jev_stats` both take a session-log path
//! directly. Neither needs a `KodEngine` on the paths this file
//! covers:
//!
//! - `run_replay(execute=false)` reads the log and prints a dry run.
//! - `run_replay(execute=true, yes=false)` on a log with a destructive
//!   tool call is refused *before* the engine is built — the guard is
//!   purely a function of the log contents and the `--yes` flag.
//! - `run_jev_stats` reads the log and counts `JevDecision` entries,
//!   with no engine.
//!
//! Each test writes a real session log to a `tempfile::TempDir` with
//! a hand-built `SessionEntry`, so `read_session` parses it cleanly.
use kod_cli::commands::admin::run_jev_stats;
use kod_cli::commands::memory::run_replay;
use kod_core_state::session_log::SessionEntry;

/// A `ToolCall` entry. `tool_name` determines the destructive check
/// in `run_replay`: the read-only allowlist includes `read_file` and
/// `grep`; anything else (e.g. `write_file`) requires `--yes`.
fn tool_call(tool_name: &str) -> SessionEntry {
    SessionEntry::ToolCall {
        timestamp_ms: 1_700_000_000_000,
        holder: "session".to_string(),
        tool_name: tool_name.to_string(),
        arguments: serde_json::json!({"path": "src/main.rs"}),
        duration_ms: 5,
        result: serde_json::json!({"success": {"ok": true}}),
    }
}

/// Write each entry as one JSON line. `read_session` reads JSONL.
fn write_log(path: &std::path::Path, entries: &[SessionEntry]) {
    let mut s = String::new();
    for e in entries {
        s.push_str(&serde_json::to_string(e).expect("serialize entry"));
        s.push('\n');
    }
    std::fs::write(path, s).expect("write session log");
}

// ---- memory::run_replay ------------------------------------------------

/// A missing log is an error — `read_session` propagates the IO error,
/// and the caller sees it rather than a silent success.
#[tokio::test]
async fn replay_with_missing_log_errors() {
    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("does-not-exist.jsonl");
    let err = run_replay(path, false, false)
        .await
        .expect_err("a missing session log must be an error");
    let msg = err.to_string();
    assert!(
        msg.contains("No such file") || msg.contains("not found") || msg.contains("IO error"),
        "unexpected error: {msg}",
    );
}

/// An empty log has no tool calls. The dry-run branch prints "No tool
/// calls" and returns Ok — "nothing to replay" is not an error.
#[tokio::test]
async fn replay_empty_log_is_ok() {
    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("empty.jsonl");
    std::fs::write(&path, "").unwrap();
    run_replay(path, false, false)
        .await
        .expect("an empty log is the first-run case, not an error");
}

/// A log with one read-only tool call, dry-run. The dry-run branch
/// enumerates the calls and returns Ok without building an engine.
#[tokio::test]
async fn replay_read_only_call_dry_run_is_ok() {
    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("read.jsonl");
    write_log(&path, &[tool_call("read_file")]);
    run_replay(path, false, false)
        .await
        .expect("dry run of a read-only call must succeed");
}

/// **The `--yes` gate, before the engine.** A log with a destructive
/// call, `execute=true`, `yes=false` returns `PermissionDenied`
/// *without* building a `KodEngine` — the check is a pure function of
/// the log's tool names and the flag. If the check moved after the
/// engine build, this test would need a config and a mock; the fact
/// that it does not is the point.
#[tokio::test]
async fn replay_destructive_without_yes_is_refused() {
    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("write.jsonl");
    write_log(&path, &[tool_call("write_file")]);
    let err = run_replay(path, true, false)
        .await
        .expect_err("a destructive call without --yes must be refused");
    let msg = err.to_string();
    assert!(
        msg.contains("--yes") || msg.contains("PermissionDenied") || msg.contains("permission"),
        "unexpected error: {msg}",
    );
}

// ---- admin::run_jev_stats ----------------------------------------------

/// A missing log is an error, same as `run_replay`.
#[tokio::test]
async fn jev_stats_with_missing_log_errors() {
    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("does-not-exist.jsonl");
    let err = run_jev_stats(Some(path))
        .await
        .expect_err("a missing session log must be an error");
    let msg = err.to_string();
    assert!(
        msg.contains("No such file") || msg.contains("not found") || msg.contains("IO error"),
        "unexpected error: {msg}",
    );
}

/// A log with no `JevDecision` entries is Ok. "No Jev decisions
/// recorded" is not an error — it is the common case for a session
/// that never enabled Jev.
#[tokio::test]
async fn jev_stats_with_no_jev_entries_is_ok() {
    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("no-jev.jsonl");
    write_log(&path, &[tool_call("read_file")]);
    run_jev_stats(Some(path))
        .await
        .expect("a log with no Jev decisions must not be an error");
}
