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
use super::mock_llm::MockStep;

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
    // Mode normalization: the PTY delivers bytes in order, but the
    // TUI's event loop reads them as separate key events only when it
    // gets a chance to run between them. Back-to-back, the `Esc` and
    // `i` can land in one poll and the `i` is typed as literal text
    // (observed as `i/export ...`). Fixed sleeps here were the source
    // of the recurring TUI e2e flake; wait for the rendered mode hint
    // instead — normal mode shows "Press i to type", insert mode shows
    // "Type a message" — so the next key is only sent once the TUI has
    // actually switched modes.
    session.send_key(KeyCode::Esc);
    wait_ci(session, "Press i to type", Duration::from_secs(5));
    session.send_key(KeyCode::Char('i'));
    wait_ci(session, "Type a message", Duration::from_secs(5));
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

// ---------------------------------------------------------------------------
// Tool-call loop (scripted provider)
// ---------------------------------------------------------------------------

/// The mock serves a tool call, the engine runs it, the model is
/// called again and answers. This is the agentic turn end to end
/// through the TUI: a `read_file` tool row must appear, then the
/// final text.
#[test]
fn tool_call_round_trip_renders_a_tool_row() {
    let env = TestEnv::new_scripted(vec![
        MockStep::ToolCall {
            name: "list_files".to_string(),
            arguments: serde_json::json!({ "path": ".", "recursive": false }).to_string(),
        },
        MockStep::Text("I listed the files.".to_string()),
    ]);
    let session = env.spawn(100, 40);
    harness::wait_for_ready(&session);

    session.send_key(KeyCode::Esc);
    session.send_key(KeyCode::Char('i'));
    session.send_text("what files are here?");
    session.send_key(KeyCode::Enter);

    // The tool row appears while the tool runs; the final text after.
    let screen = wait_ci(&session, "I listed the files.", Duration::from_secs(20));
    assert!(
        screen.to_lowercase().contains("list_files") || screen.to_lowercase().contains("tool"),
        "a tool row should render for the list_files call:\n{screen}"
    );
}

/// A failed tool call must render an error row and let the model
/// continue — the agentic loop does not abort on a tool error.
#[test]
fn a_failing_tool_call_is_surfaced_and_the_turn_continues() {
    let env = TestEnv::new_scripted(vec![
        MockStep::ToolCall {
            name: "read_file".to_string(),
            arguments: serde_json::json!({ "path": "does/not/exist.txt" }).to_string(),
        },
        MockStep::Text("That file does not exist.".to_string()),
    ]);
    let session = env.spawn(100, 40);
    harness::wait_for_ready(&session);

    session.send_key(KeyCode::Esc);
    session.send_key(KeyCode::Char('i'));
    session.send_text("read a missing file");
    session.send_key(KeyCode::Enter);

    let screen = wait_ci(&session, "does not exist", Duration::from_secs(20));
    assert!(
        screen.to_lowercase().contains("read_file") || screen.to_lowercase().contains("error"),
        "the failed tool call should leave a visible row:\n{screen}"
    );
}

/// Two tool calls back to back in one turn: the script serves call,
/// call, text. Both rows must render and the final text must arrive.
#[test]
fn two_tool_calls_in_one_turn_both_render() {
    let env = TestEnv::new_scripted(vec![
        MockStep::ToolCall {
            name: "list_files".to_string(),
            arguments: serde_json::json!({ "path": ".", "recursive": false }).to_string(),
        },
        MockStep::ToolCall {
            name: "list_files".to_string(),
            arguments: serde_json::json!({ "path": "src", "recursive": false }).to_string(),
        },
        MockStep::Text("Listed twice.".to_string()),
    ]);
    let session = env.spawn(100, 40);
    harness::wait_for_ready(&session);

    session.send_key(KeyCode::Esc);
    session.send_key(KeyCode::Char('i'));
    session.send_text("list twice");
    session.send_key(KeyCode::Enter);

    let screen = wait_ci(&session, "Listed twice", Duration::from_secs(20));
    let hits = screen.matches("list_files").count();
    assert!(
        hits >= 2,
        "two tool calls should produce at least two rows, found {hits}:\n{screen}"
    );
}

/// A `write_file` tool call must actually run: the file exists after.
#[test]
fn a_write_file_tool_call_creates_the_file() {
    let env = TestEnv::new_scripted(vec![
        MockStep::ToolCall {
            name: "write_file".to_string(),
            arguments: serde_json::json!({
                "path": "written.txt",
                "content": "hello from the tool\n"
            })
            .to_string(),
        },
        MockStep::Text("Wrote the file.".to_string()),
    ]);
    let session = env.spawn(100, 40);
    harness::wait_for_ready(&session);

    session.send_key(KeyCode::Esc);
    session.send_key(KeyCode::Char('i'));
    session.send_text("write a file");
    session.send_key(KeyCode::Enter);

    // `write_file` requires approval under the standard preset; the
    // dialog must appear before the call runs.
    wait_ci(&session, "approval required", Duration::from_secs(15));
    session.send_key(KeyCode::Char('y'));

    wait_ci(&session, "Wrote the file", Duration::from_secs(20));
    assert!(
        env.workspace.join("written.txt").exists(),
        "the approved write_file call must have created the file",
    );
}

/// `/retry` after a completed turn re-sends the last prompt; the mock
/// answers the repeated last step, so a second reply appears.
#[test]
fn retry_after_a_turn_resends_the_prompt() {
    let env = TestEnv::new_with_reply("First answer.");
    let session = env.spawn(100, 40);
    harness::wait_for_ready(&session);

    session.send_key(KeyCode::Esc);
    session.send_key(KeyCode::Char('i'));
    session.send_text("hello");
    session.send_key(KeyCode::Enter);
    wait_ci(&session, "First answer", Duration::from_secs(20));

    session.send_key(KeyCode::Esc);
    session.send_key(KeyCode::Char('i'));
    session.send_text("/retry");
    session.send_key(KeyCode::Enter);
    // `wait_ci("First answer")` would return at once — that text is
    // already on screen from the first turn. Wait for the *second*
    // occurrence by polling the count.
    let start = std::time::Instant::now();
    loop {
        let text = session.screen_text();
        let hits = text.matches("First answer").count();
        if hits >= 2 {
            break;
        }
        if start.elapsed() > Duration::from_secs(20) {
            panic!("retry produced {hits} repl(ies), expected 2:\n{text}");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

// ---------------------------------------------------------------------------
// Approval flow
// ---------------------------------------------------------------------------

/// A `write_file` under the standard preset must prompt, and `n` must
/// deny it — the file is never created.
#[test]
fn denying_a_write_leaves_no_file() {
    let env = TestEnv::new_scripted(vec![
        MockStep::ToolCall {
            name: "write_file".to_string(),
            arguments: serde_json::json!({
                "path": "denied.txt",
                "content": "should not exist\n"
            })
            .to_string(),
        },
        MockStep::Text("Understood, I will not write it.".to_string()),
    ]);
    let session = env.spawn(100, 40);
    harness::wait_for_ready(&session);

    session.send_key(KeyCode::Esc);
    session.send_key(KeyCode::Char('i'));
    session.send_text("write a file I will deny");
    session.send_key(KeyCode::Enter);

    wait_ci(&session, "approval required", Duration::from_secs(15));
    session.send_key(KeyCode::Char('n'));

    // The denial is fed back to the model, which answers.
    wait_ci(&session, "Understood", Duration::from_secs(20));
    assert!(
        !env.workspace.join("denied.txt").exists(),
        "a denied write must not create the file",
    );
}

/// Esc denies the whole batch. Same invariant as `n` for a single item.
#[test]
fn escape_denies_the_approval() {
    let env = TestEnv::new_scripted(vec![
        MockStep::ToolCall {
            name: "write_file".to_string(),
            arguments: serde_json::json!({
                "path": "escaped.txt",
                "content": "nope\n"
            })
            .to_string(),
        },
        MockStep::Text("Cancelled.".to_string()),
    ]);
    let session = env.spawn(100, 40);
    harness::wait_for_ready(&session);

    session.send_key(KeyCode::Esc);
    session.send_key(KeyCode::Char('i'));
    session.send_text("write, then I press escape");
    session.send_key(KeyCode::Enter);

    wait_ci(&session, "approval required", Duration::from_secs(15));
    session.send_key(KeyCode::Esc);

    wait_ci(&session, "Cancelled", Duration::from_secs(20));
    assert!(
        !env.workspace.join("escaped.txt").exists(),
        "an Esc-denied write must not create the file",
    );
}

/// The approval dialog names the tool and shows the key legend.
#[test]
fn the_approval_dialog_shows_the_tool_and_legend() {
    let env = TestEnv::new_scripted(vec![
        MockStep::ToolCall {
            name: "write_file".to_string(),
            arguments: serde_json::json!({
                "path": "shown.txt",
                "content": "hi\n"
            })
            .to_string(),
        },
        MockStep::Text("Done.".to_string()),
    ]);
    let session = env.spawn(110, 40);
    harness::wait_for_ready(&session);

    session.send_key(KeyCode::Esc);
    session.send_key(KeyCode::Char('i'));
    session.send_text("write a file");
    session.send_key(KeyCode::Enter);

    let screen = wait_ci(&session, "approval required", Duration::from_secs(15));
    assert!(
        screen.contains("write_file"),
        "dialog names the tool:\n{screen}"
    );
    assert!(
        screen.contains("y approve") && screen.contains("n / Esc deny"),
        "dialog shows the key legend:\n{screen}"
    );
    assert!(
        screen.contains("shown.txt"),
        "dialog names the target path:\n{screen}"
    );
    // Leave the dialog so the child does not linger.
    session.send_key(KeyCode::Esc);
}

// ---------------------------------------------------------------------------
// Provider errors
// ---------------------------------------------------------------------------

/// A provider error before any content must surface as a friendly
/// error message, not a silent no-op or a hang.
#[test]
fn a_provider_error_is_surfaced() {
    let env = TestEnv::new_scripted(vec![MockStep::Error {
        status: 500,
        message: "internal boom".to_string(),
    }]);
    let session = env.spawn(100, 40);
    harness::wait_for_ready(&session);

    session.send_key(KeyCode::Esc);
    session.send_key(KeyCode::Char('i'));
    session.send_text("trigger an error");
    session.send_key(KeyCode::Enter);

    // The TUI must leave the "generating" state and show something.
    let screen = wait_ci(&session, "error", Duration::from_secs(20));
    let lower = screen.to_lowercase();
    assert!(
        lower.contains("error") || lower.contains("failed") || lower.contains("boom"),
        "a provider error must be visible:\n{screen}"
    );
}

// ---------------------------------------------------------------------------
// Plan mode
// ---------------------------------------------------------------------------

/// `/plan-mode` reports its state; toggling flips it.
#[test]
fn plan_mode_reports_and_toggles() {
    let env = TestEnv::new();
    let session = env.spawn(90, 30);
    harness::wait_for_ready(&session);

    let screen = run_command(&session, "/plan-mode show", "plan mode");
    assert!(screen.to_lowercase().contains("plan mode"));

    // Toggle on, then query. `run_command` normalizes mode itself.
    let _ = run_command(&session, "/plan-mode on", "plan mode");
    let screen = run_command(&session, "/plan-mode show", "plan mode: on");
    assert!(
        screen.to_lowercase().contains("plan mode: on"),
        "plan mode should be on after toggling:\n{screen}"
    );
}

/// `/plan` with no plan says so.
#[test]
fn plan_without_a_plan_says_so() {
    let env = TestEnv::new();
    let session = env.spawn(90, 30);
    harness::wait_for_ready(&session);
    let screen = run_command(&session, "/plan", "no plan");
    assert!(screen.to_lowercase().contains("no plan"));
}

// ---------------------------------------------------------------------------
// Session persistence
// ---------------------------------------------------------------------------

/// `/save` writes a file, `/load` reads it back, and the messages
/// round-trip.
#[test]
fn save_and_load_round_trip_a_session() {
    let env = TestEnv::new();
    let session = env.spawn(100, 40);
    harness::wait_for_ready(&session);

    // Have a turn first so there is something to save.
    session.send_key(KeyCode::Esc);
    session.send_key(KeyCode::Char('i'));
    session.send_text("hello there");
    session.send_key(KeyCode::Enter);
    wait_ci(&session, "Hi from the mock", Duration::from_secs(20));

    // /export --json produces a loadable file.
    let path = env.workspace.join("session.json");
    std::thread::sleep(Duration::from_millis(300));
    session.send_key(KeyCode::Esc);
    std::thread::sleep(Duration::from_millis(80));
    session.send_key(KeyCode::Char('i'));
    std::thread::sleep(Duration::from_millis(80));
    session.send_text(&format!("/export --json {}", path.display()));
    session.send_key(KeyCode::Enter);
    // Poll rather than sleep a fixed 800ms: the write is async, and no
    // fixed sleep is reliable under parallel load. This was the flake.
    assert!(
        harness::wait_for_file(&path, Duration::from_secs(20)),
        "/export --json must write the file ({} missing); screen:\n{}",
        path.display(),
        session.screen_text()
    );

    // /load reads it back.
    std::thread::sleep(Duration::from_millis(200));
    session.send_key(KeyCode::Esc);
    std::thread::sleep(Duration::from_millis(80));
    session.send_key(KeyCode::Char('i'));
    std::thread::sleep(Duration::from_millis(80));
    session.send_text(&format!("/load {}", path.display()));
    session.send_key(KeyCode::Enter);
    let screen = wait_ci(&session, "loaded", Duration::from_secs(10));
    assert!(screen.to_lowercase().contains("loaded"));
}

/// `/branch` drops a marker naming the message count.
#[test]
fn branch_drops_a_marker() {
    let env = TestEnv::new();
    let session = env.spawn(90, 30);
    harness::wait_for_ready(&session);
    let screen = run_command(&session, "/branch my-checkpoint", "branch point");
    assert!(
        screen.contains("my-checkpoint"),
        "branch names its label:\n{screen}"
    );
}

// ---------------------------------------------------------------------------
// Swarm
// ---------------------------------------------------------------------------

/// `/swarm <goal>` decomposes, runs agents, and merges. The mock
/// serves the decompose JSON, then per-agent text, then a synthesis.
#[test]
fn swarm_runs_and_merges() {
    // Decompose is the first request (prompt contains "Split this
    // goal"); each agent and the merge follow.
    let env = TestEnv::new_scripted(vec![
        MockStep::Text(r#"[{"name":"a","description":"do a","expected_writes":[]}]"#.to_string()),
        MockStep::Text("agent a done".to_string()),
        MockStep::Text("MERGED result".to_string()),
    ]);
    let session = env.spawn(120, 40);
    harness::wait_for_ready(&session);

    session.send_key(KeyCode::Esc);
    session.send_key(KeyCode::Char('i'));
    session.send_text("/swarm build a thing");
    session.send_key(KeyCode::Enter);

    // The swarm announces it started; the merged result follows.
    let _ = wait_ci(&session, "starting swarm", Duration::from_secs(25));
    let screen = wait_ci(&session, "MERGED", Duration::from_secs(25));
    assert!(
        screen.to_lowercase().contains("swarm") || screen.contains("MERGED"),
        "swarm should report its run and result:\n{screen}"
    );
}

/// `/swarm` with no goal prints usage.
#[test]
fn swarm_without_a_goal_shows_usage() {
    let env = TestEnv::new();
    let session = env.spawn(100, 40);
    harness::wait_for_ready(&session);
    let screen = run_command(&session, "/swarm", "usage: /swarm");
    assert!(screen.contains("/swarm"));
}

// ---------------------------------------------------------------------------
// Multi-item approval batch
// ---------------------------------------------------------------------------

/// Two `write_file` calls in one response produce a single two-item
/// dialog. Approving both creates both files.
#[test]
fn a_two_item_batch_approves_both() {
    let env = TestEnv::new_scripted(vec![
        MockStep::ToolCalls(vec![
            (
                "write_file".to_string(),
                serde_json::json!({ "path": "first.txt", "content": "one\n" }).to_string(),
            ),
            (
                "write_file".to_string(),
                serde_json::json!({ "path": "second.txt", "content": "two\n" }).to_string(),
            ),
        ]),
        MockStep::Text("Both written.".to_string()),
    ]);
    let session = env.spawn(110, 42);
    harness::wait_for_ready(&session);

    session.send_key(KeyCode::Esc);
    session.send_key(KeyCode::Char('i'));
    session.send_text("write two files");
    session.send_key(KeyCode::Enter);

    // First item.
    wait_ci(&session, "approval required", Duration::from_secs(15));
    wait_ci(&session, "first.txt", Duration::from_secs(10));
    session.send_key(KeyCode::Char('y'));
    // Wait for the second item's path before deciding it.
    wait_ci(&session, "second.txt", Duration::from_secs(10));
    session.send_key(KeyCode::Char('y'));

    wait_ci(&session, "Both written", Duration::from_secs(20));
    assert!(
        env.workspace.join("first.txt").exists(),
        "first approved file"
    );
    assert!(
        env.workspace.join("second.txt").exists(),
        "second approved file"
    );
}

/// Approve the first item, deny the second: only the first is created.
#[test]
fn a_two_item_batch_can_mix_decisions() {
    let env = TestEnv::new_scripted(vec![
        MockStep::ToolCalls(vec![
            (
                "write_file".to_string(),
                serde_json::json!({ "path": "keep.txt", "content": "keep\n" }).to_string(),
            ),
            (
                "write_file".to_string(),
                serde_json::json!({ "path": "drop.txt", "content": "drop\n" }).to_string(),
            ),
        ]),
        MockStep::Text("Handled.".to_string()),
    ]);
    let session = env.spawn(110, 42);
    harness::wait_for_ready(&session);

    session.send_key(KeyCode::Esc);
    session.send_key(KeyCode::Char('i'));
    session.send_text("write two, deny the second");
    session.send_key(KeyCode::Enter);

    wait_ci(&session, "approval required", Duration::from_secs(15));
    // Approve the current item, then deny the next. The batch order is
    // not deterministic — the engine builds it from a `HashSet` — so
    // the test does not assume which path is first. The invariant is
    // "exactly one approved, one denied".
    session.send_key(KeyCode::Char('y'));
    std::thread::sleep(Duration::from_millis(500));
    session.send_key(KeyCode::Char('n'));

    wait_ci(&session, "Handled", Duration::from_secs(20));
    let keep = env.workspace.join("keep.txt").exists();
    let drop = env.workspace.join("drop.txt").exists();
    assert!(
        keep ^ drop,
        "exactly one file must exist (approved) and one must not (denied); \
         keep={keep} drop={drop}",
    );
}

/// Esc on the first item denies both (the whole batch).
#[test]
fn escape_denies_a_whole_batch() {
    let env = TestEnv::new_scripted(vec![
        MockStep::ToolCalls(vec![
            (
                "write_file".to_string(),
                serde_json::json!({ "path": "a.txt", "content": "a\n" }).to_string(),
            ),
            (
                "write_file".to_string(),
                serde_json::json!({ "path": "b.txt", "content": "b\n" }).to_string(),
            ),
        ]),
        MockStep::Text("All denied.".to_string()),
    ]);
    let session = env.spawn(110, 42);
    harness::wait_for_ready(&session);

    session.send_key(KeyCode::Esc);
    session.send_key(KeyCode::Char('i'));
    session.send_text("write two, then escape");
    session.send_key(KeyCode::Enter);

    wait_ci(&session, "approval required", Duration::from_secs(15));
    session.send_key(KeyCode::Esc);

    wait_ci(&session, "All denied", Duration::from_secs(20));
    assert!(!env.workspace.join("a.txt").exists(), "Esc denied item 1");
    assert!(!env.workspace.join("b.txt").exists(), "Esc denied item 2");
}

// ---------------------------------------------------------------------------
// Swarm agent panel
// ---------------------------------------------------------------------------

/// After a swarm runs, `a` opens the agent panel and it names the
/// agents that ran. The empty-panel state is also asserted (before the
/// panel has agents, it says so).
#[test]
fn the_agent_panel_lists_swarm_agents() {
    let env = TestEnv::new_scripted(vec![
        MockStep::Text(
            r#"[{"name":"alpha","description":"do alpha","expected_writes":[]},{"name":"beta","description":"do beta","expected_writes":[]}]"#
                .to_string(),
        ),
        MockStep::Text("alpha finished".to_string()),
        MockStep::Text("beta finished".to_string()),
        MockStep::Text("MERGED both".to_string()),
    ]);
    let session = env.spawn(120, 44);
    harness::wait_for_ready(&session);

    // Per-key settling: back-to-back keys can land in one poll.
    std::thread::sleep(Duration::from_millis(200));
    session.send_key(KeyCode::Esc);
    std::thread::sleep(Duration::from_millis(80));
    session.send_key(KeyCode::Char('i'));
    std::thread::sleep(Duration::from_millis(80));
    session.send_text("/swarm two things");
    session.send_key(KeyCode::Enter);

    // Let the swarm actually run: the merged text is the last thing it
    // prints, after decompose + both agents + the merge.
    wait_ci(&session, "MERGED", Duration::from_secs(40));

    // Open the agent panel (normal mode `a`).
    session.send_key(KeyCode::Esc);
    std::thread::sleep(Duration::from_millis(200));
    session.send_key(KeyCode::Char('a'));

    // The panel is a 30%-wide column. It must name an agent that ran —
    // "no active agents" is the *empty* state, which would mean the
    // swarm panel never populated.
    let screen = wait_ci(&session, "Agents", Duration::from_secs(10));
    let lower = screen.to_lowercase();
    assert!(
        lower.contains("alpha") || lower.contains("beta"),
        "the agent panel must list a swarm agent (not the empty state):\n{screen}"
    );
}

/// Before any swarm runs, the panel says there are no active agents.
#[test]
fn the_agent_panel_is_empty_before_a_swarm() {
    let env = TestEnv::new();
    let session = env.spawn(120, 44);
    harness::wait_for_ready(&session);

    session.send_key(KeyCode::Esc);
    std::thread::sleep(Duration::from_millis(200));
    session.send_key(KeyCode::Char('a'));

    let screen = wait_ci(&session, "Agents", Duration::from_secs(10));
    assert!(
        screen.to_lowercase().contains("no active agents"),
        "an idle panel should say it is empty:\n{screen}"
    );
}
