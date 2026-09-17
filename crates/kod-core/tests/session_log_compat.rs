//! Session log backward- and forward-compatibility (§11.3, AD-15).
//!
//! The session log is the file `kod replay` reads. Its format has
//! grown since the first release: `ToolCall` was the only variant,
//! then `ModelFallback`, `PolicyDecision`, `Cost`, `MemoryWrite`,
//! `Approval`, and `Diagnostics` joined it. Two properties must
//! hold for the format to be safe to extend:
//!
//! 1. **Backward compatibility**: a file written by an older build
//!    — only `ToolCall` lines — must parse and replay today.
//! 2. **Forward compatibility**: a file written by a newer build
//!    — a `kind` this build does not recognise — must parse, with
//!    the unknown line skipped and a warning rather than a hard
//!    error. A user who downgrades should not lose access to their
//!    session history because the file contains a line from a
//!    future release.
//!
//! A third property is deliberately *not* relaxed: a line that is
//! not valid JSON is still a hard error. Skipping a malformed line
//! would hide a real truncation — a crash mid-write, a partial
//! copy, a file mangled by an editor.

use kod_core::session_log::{SessionEntry, read_session};
use tempfile::TempDir;

/// One old-format `ToolCall` line, verbatim. Matches what an earlier
/// build wrote — no new fields, no new variants.
const OLD_TOOL_CALL_LINE: &str = r#"{"kind":"tool_call","timestamp_ms":1700000000000,"holder":"session","tool_name":"read_file","arguments":{"path":"src/main.rs"},"duration_ms":7,"result":{"success":{"path":"src/main.rs","content":"fn main() {}"}}}"#;

#[test]
fn old_format_file_parses_under_todays_reader() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("old.jsonl");
    let contents = format!("{OLD_TOOL_CALL_LINE}\n{OLD_TOOL_CALL_LINE}\n");
    std::fs::write(&path, contents).unwrap();

    let entries = read_session(&path).expect("old format must parse");
    assert_eq!(entries.len(), 2);
    for e in &entries {
        match e {
            SessionEntry::ToolCall { tool_name, holder, .. } => {
                assert_eq!(tool_name, "read_file");
                assert_eq!(holder, "session");
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }
}

#[test]
fn mixed_format_file_parses_every_known_kind() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("mixed.jsonl");

    let lines = [
        OLD_TOOL_CALL_LINE.to_string(),
        r#"{"kind":"model_fallback","timestamp_ms":1,"holder":"s","from":"a","to":"b","error":"timeout"}"#.to_string(),
        r#"{"kind":"policy_decision","timestamp_ms":2,"holder":"s","tool_name":"write_file","outcome":"ask","rule":"preset","source":"preset"}"#.to_string(),
        r#"{"kind":"cost","timestamp_ms":3,"holder":"s","endpoint":"e","model":"m","prompt_tokens":10,"completion_tokens":5,"cost_usd":0.0015}"#.to_string(),
        r#"{"kind":"memory_write","timestamp_ms":4,"memory_id":"abc","channel":"extraction","tags":["auto-fact"]}"#.to_string(),
        r#"{"kind":"approval","timestamp_ms":5,"holder":"s","tool_name":"write_file","decision":"approve"}"#.to_string(),
        r#"{"kind":"diagnostics","timestamp_ms":6,"file":"src/x.rs","error_count":2,"warning_count":1}"#.to_string(),
    ];
    std::fs::write(&path, lines.join("\n") + "\n").unwrap();

    let entries = read_session(&path).expect("mixed format must parse");
    assert_eq!(entries.len(), 7, "every known kind must round-trip");

    assert!(matches!(entries[0], SessionEntry::ToolCall { .. }));
    assert!(matches!(entries[1], SessionEntry::ModelFallback { .. }));
    assert!(matches!(entries[2], SessionEntry::PolicyDecision { .. }));
    assert!(matches!(entries[3], SessionEntry::Cost { .. }));
    assert!(matches!(entries[4], SessionEntry::MemoryWrite { .. }));
    assert!(matches!(entries[5], SessionEntry::Approval { .. }));
    assert!(matches!(entries[6], SessionEntry::Diagnostics { .. }));
}

#[test]
fn unknown_kind_is_skipped_not_an_error() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("future.jsonl");

    let contents = format!(
        "{OLD_TOOL_CALL_LINE}\n\
         {{\"kind\":\"future_thing\",\"timestamp_ms\":9,\"anything\":42}}\n\
         {OLD_TOOL_CALL_LINE}\n"
    );
    std::fs::write(&path, contents).unwrap();

    let entries = read_session(&path).expect("unknown kind must not error");
    assert_eq!(
        entries.len(),
        2,
        "the unknown kind is skipped, the two tool calls survive",
    );
    for e in &entries {
        assert!(matches!(e, SessionEntry::ToolCall { .. }));
    }
}

#[test]
fn malformed_line_is_still_a_hard_error() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("corrupt.jsonl");
    std::fs::write(&path, "{not valid json\n").unwrap();

    let err = read_session(&path).expect_err("malformed JSON must error");
    let msg = err.to_string();
    assert!(msg.contains("line 1"), "error should name the line: {msg}");
}

#[test]
fn empty_lines_are_ignored() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("empty.jsonl");
    let contents = format!("\n\n{OLD_TOOL_CALL_LINE}\n\n");
    std::fs::write(&path, contents).unwrap();

    let entries = read_session(&path).expect("empty lines must not error");
    assert_eq!(entries.len(), 1);
}
