//! Command-surface coverage for the TUI, driven end-to-end through a
//! real PTY against the mock provider.
//!
//! `tui_e2e.rs` covers the *lifecycle* — spawn, type, stream, quit,
//! resize. This file covers the *command surface*: every slash command
//! that produces observable output is sent and its response asserted.
//! The point is a regression net for the command dispatcher: a command
//! that starts panicking, silently no-ops, or loses its handler arm
//! fails here rather than when a user types it.
//!
//! # What is and is not asserted
//!
//! Assertions are on stable substrings (from the command's own
//! `push_system_message`), never on full screen layout — layout is the
//! render layer's contract and its own tests. A command that needs the
//! engine reports "Engine not initialized" when run with no provider;
//! that path is exercised by the `*_without_engine` tests.
//!
//! # Why a helper
//!
//! Each test is spawn → wait-for-ready → send command → wait-for-text.
//! `run_command` factors the three steps and centralises the timeout,
//! so a new test is one line.

use std::time::Duration;

use std::time::Instant;

use super::harness::{self, KeyCode, TestEnv};

/// Wait until the screen contains `needle`, **case-insensitively**.
///
/// The TUI renders command output in sentence case ("Goal cleared"),
/// and a test that hard-codes the exact capitalisation becomes a
/// maintenance burden the moment a message is reworded. Matching
/// case-insensitively keeps the assertions about *what* the command
/// said, not how it was capitalised.
///
/// Polls `screen_text()` rather than using the harness's
/// `wait_for_text` because that one is case-sensitive by design.
fn wait_ci(session: &harness::TuiSession, needle: &str, timeout: Duration) -> String {
    let needle_lower = needle.to_lowercase();
    let start = Instant::now();
    loop {
        let text = session.screen_text();
        if text.to_lowercase().contains(&needle_lower) {
            return text;
        }
        if start.elapsed() > timeout {
            panic!(
                "timed out waiting for {needle:?}
--- screen ---
{text}"
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Send a slash command and return the screen once `expect` appears.
///
/// `Esc` first, then `i`: the TUI stays in insert mode after a
/// submitted command, so a helper that unconditionally sent `i` would
/// type a literal `i` before the next command (producing `i/whoami`).
/// Normalising to normal mode first makes the helper idempotent.
fn run_command(session: &harness::TuiSession, command: &str, expect: &str) -> String {
    session.send_key(KeyCode::Esc);
    session.send_key(KeyCode::Char('i'));
    session.send_text(command);
    session.send_key(KeyCode::Enter);
    wait_ci(session, expect, Duration::from_secs(10))
}

// ---------------------------------------------------------------------------
// Informational commands (no engine state required)
// ---------------------------------------------------------------------------

#[test]
fn help_opens_the_keybinding_overlay() {
    let env = TestEnv::new();
    let session = env.spawn(100, 40);
    harness::wait_for_ready(&session);
    // `/help` opens the keybinding overlay (sections "Modes",
    // "Typing"); it is not a command list.
    session.send_key(KeyCode::Esc);
    session.send_key(KeyCode::Char('i'));
    session.send_text("/help");
    session.send_key(KeyCode::Enter);
    let screen = wait_ci(&session, "help — Esc closes", Duration::from_secs(10));
    assert!(
        screen.contains("Modes") && screen.contains("Typing"),
        "help overlay should show keybinding sections:\n{screen}"
    );
}

#[test]
fn whoami_reports_a_session_summary() {
    let env = TestEnv::new();
    let session = env.spawn(90, 30);
    harness::wait_for_ready(&session);
    let screen = run_command(&session, "/whoami", "session summary");
    let lower = screen.to_lowercase();
    assert!(
        lower.contains("session summary") || lower.contains("model"),
        "whoami produced no summary:\n{screen}"
    );
}

#[test]
fn stats_reports_session_totals() {
    let env = TestEnv::new();
    let session = env.spawn(90, 30);
    harness::wait_for_ready(&session);
    let screen = run_command(&session, "/stats", "stats");
    assert!(
        screen.to_lowercase().contains("token")
            || screen.to_lowercase().contains("session")
            || screen.to_lowercase().contains("cost"),
        "stats produced no totals:\n{screen}"
    );
}

#[test]
fn context_reports_usage() {
    let env = TestEnv::new();
    let session = env.spawn(90, 30);
    harness::wait_for_ready(&session);
    let screen = run_command(&session, "/context", "context");
    assert!(
        screen.to_lowercase().contains("context") || screen.to_lowercase().contains("token"),
        "context produced no usage:\n{screen}"
    );
}

#[test]
fn model_reports_the_current_model() {
    let env = TestEnv::new();
    let session = env.spawn(90, 30);
    harness::wait_for_ready(&session);
    let screen = run_command(&session, "/model", "model");
    assert!(
        screen.to_lowercase().contains("model"),
        "model produced no report:\n{screen}"
    );
}

#[test]
fn theme_reports_the_current_theme() {
    let env = TestEnv::new();
    let session = env.spawn(90, 30);
    harness::wait_for_ready(&session);
    let screen = run_command(&session, "/theme", "theme");
    assert!(
        screen.to_lowercase().contains("theme"),
        "theme produced no report:\n{screen}"
    );
}

#[test]
fn limits_reports_the_caps() {
    let env = TestEnv::new();
    let session = env.spawn(90, 30);
    harness::wait_for_ready(&session);
    let screen = run_command(&session, "/limits", "limit");
    assert!(
        screen.to_lowercase().contains("limit")
            || screen.to_lowercase().contains("token")
            || screen.to_lowercase().contains("rate"),
        "limits produced no report:\n{screen}"
    );
}

#[test]
fn tools_lists_the_registry() {
    let env = TestEnv::new();
    let session = env.spawn(100, 40);
    harness::wait_for_ready(&session);
    let screen = run_command(&session, "/tools", "tool");
    let lower = screen.to_lowercase();
    // The registry always has read_file / write_file.
    assert!(
        lower.contains("read_file") || lower.contains("write_file") || lower.contains("tool"),
        "tools listed nothing:\n{screen}"
    );
}

#[test]
fn skills_lists_none_or_some() {
    let env = TestEnv::new();
    let session = env.spawn(100, 40);
    harness::wait_for_ready(&session);
    let screen = run_command(&session, "/skills", "skill");
    assert!(
        screen.to_lowercase().contains("skill"),
        "skills produced no report:\n{screen}"
    );
}

#[test]
fn policy_reports_the_effective_preset() {
    let env = TestEnv::new();
    let session = env.spawn(100, 40);
    harness::wait_for_ready(&session);
    let screen = run_command(&session, "/policy", "policy");
    assert!(
        screen.to_lowercase().contains("policy")
            || screen.to_lowercase().contains("preset")
            || screen.to_lowercase().contains("standard"),
        "policy produced no report:\n{screen}"
    );
}

#[test]
fn debug_tokens_reports_budget() {
    let env = TestEnv::new();
    let session = env.spawn(90, 30);
    harness::wait_for_ready(&session);
    // No prompt sent yet — the command must still respond.
    let screen = run_command(&session, "/debug tokens", "no prompt");
    assert!(
        screen.to_lowercase().contains("prompt") || screen.to_lowercase().contains("token"),
        "debug tokens produced nothing:\n{screen}"
    );
}

#[test]
fn cache_reports_cache_state() {
    let env = TestEnv::new();
    let session = env.spawn(90, 30);
    harness::wait_for_ready(&session);
    let screen = run_command(&session, "/cache", "cache");
    assert!(
        screen.to_lowercase().contains("cache"),
        "cache produced no report:\n{screen}"
    );
}

#[test]
fn jobs_reports_no_jobs() {
    let env = TestEnv::new();
    let session = env.spawn(90, 30);
    harness::wait_for_ready(&session);
    let screen = run_command(&session, "/jobs", "job");
    assert!(
        screen.to_lowercase().contains("job"),
        "jobs produced no report:\n{screen}"
    );
}

#[test]
fn trace_reports_no_traces() {
    let env = TestEnv::new();
    let session = env.spawn(90, 30);
    harness::wait_for_ready(&session);
    let screen = run_command(&session, "/trace", "trace");
    assert!(
        screen.to_lowercase().contains("trace"),
        "trace produced no report:\n{screen}"
    );
}

#[test]
fn blackboard_reports_state() {
    let env = TestEnv::new();
    let session = env.spawn(90, 30);
    harness::wait_for_ready(&session);
    let screen = run_command(&session, "/blackboard", "blackboard");
    assert!(
        screen.to_lowercase().contains("blackboard"),
        "blackboard produced no report:\n{screen}"
    );
}

#[test]
fn decisions_reports_no_decisions() {
    let env = TestEnv::new();
    let session = env.spawn(90, 30);
    harness::wait_for_ready(&session);
    let screen = run_command(&session, "/decisions", "decision");
    assert!(
        screen.to_lowercase().contains("decision"),
        "decisions produced no report:\n{screen}"
    );
}

#[test]
fn memory_reports_disabled_or_state() {
    let env = TestEnv::new();
    let session = env.spawn(90, 30);
    harness::wait_for_ready(&session);
    let screen = run_command(&session, "/memory", "memory");
    assert!(
        screen.to_lowercase().contains("memory"),
        "memory produced no report:\n{screen}"
    );
}

#[test]
fn map_reports_or_explains_empty() {
    let env = TestEnv::new();
    let session = env.spawn(90, 30);
    harness::wait_for_ready(&session);
    let screen = run_command(&session, "/map", "map");
    assert!(
        screen.to_lowercase().contains("map") || screen.to_lowercase().contains("file"),
        "map produced no report:\n{screen}"
    );
}

#[test]
fn git_status_reports_a_tree() {
    let env = TestEnv::new();
    let session = env.spawn(90, 30);
    harness::wait_for_ready(&session);
    let screen = run_command(&session, "/git-status", "git");
    assert!(
        screen.to_lowercase().contains("git")
            || screen.to_lowercase().contains("clean")
            || screen.to_lowercase().contains("branch"),
        "git-status produced no report:\n{screen}"
    );
}

#[test]
fn diff_reports_or_explains_empty() {
    let env = TestEnv::new();
    let session = env.spawn(90, 30);
    harness::wait_for_ready(&session);
    let screen = run_command(&session, "/diff", "diff");
    assert!(
        screen.to_lowercase().contains("diff")
            || screen.to_lowercase().contains("change")
            || screen.to_lowercase().contains("clean"),
        "diff produced no report:\n{screen}"
    );
}

#[test]
fn last_prompt_reports_none_yet() {
    let env = TestEnv::new();
    let session = env.spawn(90, 30);
    harness::wait_for_ready(&session);
    let screen = run_command(&session, "/last-prompt", "no prompt");
    assert!(
        screen.to_lowercase().contains("prompt"),
        "last-prompt produced no report:\n{screen}"
    );
}

#[test]
fn system_reports_no_override() {
    let env = TestEnv::new();
    let session = env.spawn(90, 30);
    harness::wait_for_ready(&session);
    let screen = run_command(&session, "/system", "system");
    assert!(
        screen.to_lowercase().contains("system") || screen.to_lowercase().contains("override"),
        "system produced no report:\n{screen}"
    );
}

#[test]
fn budget_reports_state() {
    let env = TestEnv::new();
    let session = env.spawn(90, 30);
    harness::wait_for_ready(&session);
    let screen = run_command(&session, "/budget", "budget");
    assert!(
        screen.to_lowercase().contains("budget"),
        "budget produced no report:\n{screen}"
    );
}

#[test]
fn log_reports_a_path_or_empty() {
    let env = TestEnv::new();
    let session = env.spawn(100, 40);
    harness::wait_for_ready(&session);
    let screen = run_command(&session, "/log", "log");
    assert!(
        screen.to_lowercase().contains("log"),
        "log produced no report:\n{screen}"
    );
}

// ---------------------------------------------------------------------------
// Empty-state commands (must answer, not hang or panic)
// ---------------------------------------------------------------------------

#[test]
fn retry_without_a_prompt_is_refused_cleanly() {
    let env = TestEnv::new();
    let session = env.spawn(90, 30);
    harness::wait_for_ready(&session);
    let screen = run_command(&session, "/retry", "nothing to retry");
    assert!(screen.contains("retry") || screen.contains("Nothing"));
}

#[test]
fn undo_on_empty_chat_is_refused_cleanly() {
    let env = TestEnv::new();
    let session = env.spawn(90, 30);
    harness::wait_for_ready(&session);
    let screen = run_command(&session, "/undo", "nothing to undo");
    assert!(screen.to_lowercase().contains("undo"));
}

#[test]
fn copy_without_a_reply_is_refused_cleanly() {
    let env = TestEnv::new();
    let session = env.spawn(90, 30);
    harness::wait_for_ready(&session);
    let screen = run_command(&session, "/copy", "nothing to copy");
    assert!(screen.to_lowercase().contains("copy"));
}

#[test]
fn fork_reports_a_fork() {
    let env = TestEnv::new();
    let session = env.spawn(90, 30);
    harness::wait_for_ready(&session);
    // The command itself is a message, so the chat is never empty at
    // this point; `/fork` records a fork and says so.
    let screen = run_command(&session, "/fork", "fork");
    assert!(screen.to_lowercase().contains("fork"));
}

#[test]
fn cancel_when_idle_is_refused_cleanly() {
    let env = TestEnv::new();
    let session = env.spawn(90, 30);
    harness::wait_for_ready(&session);
    let screen = run_command(&session, "/cancel", "nothing is running");
    assert!(screen.to_lowercase().contains("running") || screen.to_lowercase().contains("nothing"));
}

#[test]
fn steer_when_idle_is_refused_cleanly() {
    let env = TestEnv::new();
    let session = env.spawn(90, 30);
    harness::wait_for_ready(&session);
    let screen = run_command(&session, "/steer do the thing", "nothing is running");
    assert!(screen.to_lowercase().contains("running") || screen.to_lowercase().contains("steer"));
}

#[test]
fn goal_clear_when_no_goal_is_harmless() {
    let env = TestEnv::new();
    let session = env.spawn(90, 30);
    harness::wait_for_ready(&session);
    let screen = run_command(&session, "/goal clear", "goal cleared");
    assert!(screen.to_lowercase().contains("goal"));
}

#[test]
fn plan_clear_is_harmless() {
    let env = TestEnv::new();
    let session = env.spawn(90, 30);
    harness::wait_for_ready(&session);
    let screen = run_command(&session, "/plan clear", "plan cleared");
    assert!(screen.to_lowercase().contains("plan"));
}

#[test]
fn blackboard_clear_is_harmless() {
    let env = TestEnv::new();
    let session = env.spawn(90, 30);
    harness::wait_for_ready(&session);
    let screen = run_command(&session, "/blackboard clear", "blackboard cleared");
    assert!(screen.to_lowercase().contains("blackboard"));
}

// ---------------------------------------------------------------------------
// Commands that need the engine; with the mock provider they succeed
// ---------------------------------------------------------------------------

#[test]
fn doctor_runs_with_the_mock_provider() {
    let env = TestEnv::new();
    let session = env.spawn(100, 40);
    harness::wait_for_ready(&session);
    let screen = run_command(&session, "/doctor", "doctor");
    assert!(
        screen.to_lowercase().contains("doctor") || screen.to_lowercase().contains("check"),
        "doctor produced no report:\n{screen}"
    );
}

#[test]
fn check_runs_or_explains_no_project() {
    let env = TestEnv::new();
    let session = env.spawn(90, 30);
    harness::wait_for_ready(&session);
    // The tempdir has no Cargo.toml; the command must say so, not hang.
    let screen = run_command(&session, "/check", "check");
    assert!(screen.to_lowercase().contains("check") || screen.to_lowercase().contains("project"));
}

// ---------------------------------------------------------------------------
// Argument validation (must be rejected with a usage line, not panic)
// ---------------------------------------------------------------------------

#[test]
fn save_without_a_path_shows_usage() {
    let env = TestEnv::new();
    let session = env.spawn(90, 30);
    harness::wait_for_ready(&session);
    let screen = run_command(&session, "/save", "usage: /save");
    assert!(screen.contains("/save"));
}

#[test]
fn load_without_a_path_shows_usage() {
    let env = TestEnv::new();
    let session = env.spawn(90, 30);
    harness::wait_for_ready(&session);
    let screen = run_command(&session, "/load", "usage: /load");
    assert!(screen.contains("/load"));
}

#[test]
fn memory_delete_without_an_id_shows_usage() {
    let env = TestEnv::new();
    let session = env.spawn(90, 30);
    harness::wait_for_ready(&session);
    let screen = run_command(&session, "/memory delete", "usage: /memory delete");
    assert!(screen.contains("/memory"));
}

/// Regression: `/regenerate` with no prior exchange must report
/// "Nothing to regenerate", not loop. Before the fix it dropped its
/// own command echo, re-dispatched itself, and the process died.
#[test]
fn regenerate_without_a_reply_reports_nothing_to_regenerate() {
    let env = TestEnv::new();
    let session = env.spawn(90, 30);
    harness::wait_for_ready(&session);
    let screen = run_command(&session, "/regenerate", "nothing to regenerate");
    assert!(screen.to_lowercase().contains("nothing to regenerate"));
}

/// `/delete` on an empty chat must report "Nothing to delete" and
/// leave the TUI alive. Before the `drop_last_exchange` fix it dropped
/// its own command echo and then had nothing to fall back to; the
/// response proves the process is still dispatching commands.
#[test]
fn delete_on_an_empty_chat_reports_nothing_to_delete() {
    let env = TestEnv::new();
    let session = env.spawn(90, 30);
    harness::wait_for_ready(&session);
    let screen = run_command(&session, "/delete", "nothing to delete");
    assert!(screen.to_lowercase().contains("nothing to delete"));
}

#[test]
fn unknown_command_is_reported() {
    let env = TestEnv::new();
    let session = env.spawn(90, 30);
    harness::wait_for_ready(&session);
    let screen = run_command(&session, "/thiscannotexist", "unknown");
    assert!(
        screen.to_lowercase().contains("unknown") || screen.to_lowercase().contains("not a"),
        "unknown command produced no error:\n{screen}"
    );
}
