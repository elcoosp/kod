//! Session log forward and backward compatibility (design §11.3,
//! AD-15).
//!
//! The `SessionEntry` enum is tagged by `kind`. Adding a new variant
//! is the design's stated extension point: an older build reading a
//! newer log must skip the unknown lines rather than fail. This file
//! pins that contract on a synthetic fixture with every current
//! variant plus one "future" entry the current build does not know.
//!
//! # Why a fixture rather than the recorder
//!
//! The recorder writes today's variants; a fixture lets us write the
//! synthetic "future" line by hand and iterate over every kind. A
//! recorder round-trip would not exercise the unknown-kind path at
//! all.

use kod_core::session_log::{SessionEntry, read_session};
use tempfile::TempDir;

/// Every current variant plus a synthetic `future_kind` line the
/// reader must skip.
const FIXTURE: &str = r#"{"kind":"tool_call","timestamp_ms":1,"holder":"session","tool_name":"read_file","arguments":{"path":"src/main.rs"},"duration_ms":3,"result":{"success":{"path":"src/main.rs","content":"fn main() {}\n"}}}
{"kind":"policy_decision","timestamp_ms":2,"holder":"session","tool_name":"write_file","outcome":"ask","rule":"preset Standard applies","source":"preset"}
{"kind":"model_fallback","timestamp_ms":3,"holder":"session","from":"local-ollama/qwen2.5-coder:7b","to":"anthropic/claude-sonnet-4-5","error":"rate limited"}
{"kind":"cost","timestamp_ms":4,"holder":"session","endpoint":"anthropic","model":"claude-sonnet-4-5","prompt_tokens":100,"completion_tokens":50,"cost_usd":0.0125}
{"kind":"memory_write","timestamp_ms":5,"memory_id":"mem-abc","channel":"extraction","tags":["auto-fact","rust"]}
{"kind":"approval","timestamp_ms":6,"holder":"session","tool_name":"write_file","decision":"approve"}
{"kind":"diagnostics","timestamp_ms":7,"file":"src/main.rs","error_count":1,"warning_count":2}
{"kind":"jev_decision","timestamp_ms":8,"holder":"session","purpose":"tool_filter","state_preview":"User request: run tests","questions_summary":"filesystem,shell","answers":{"filesystem":0.92,"shell":0.31},"confidence":0.92,"latency_ms":210,"cached":false,"source":"jev"}
{"kind":"future_kind_this_build_does_not_know","timestamp_ms":8,"payload":{"anything":42}}
{"kind":"tool_call","timestamp_ms":9,"holder":"session","tool_name":"write_file","arguments":{"path":"src/out.rs","content":"x"},"duration_ms":5,"result":{"success":{"path":"src/out.rs","written":1}}}
"#;

#[test]
fn reads_every_known_variant_and_skips_the_unknown_one() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("fixture.jsonl");
    std::fs::write(&path, FIXTURE).unwrap();

    let entries = read_session(&path).expect("read must succeed");

    // Every known line: 2 tool calls + policy + fallback + cost +
    // memory write + approval + diagnostics + jev_decision = 9. The
    // synthetic future line is skipped.
    assert_eq!(
        entries.len(),
        9,
        "expected 9 known entries; got {} — a variant was dropped or \
         the unknown-kind skip regressed",
        entries.len(),
    );

    // The tool calls came back in file order and the second is the
    // write_file (later in the file than the read_file).
    let tool_names: Vec<&str> = entries
        .iter()
        .filter_map(|e| match e {
            SessionEntry::ToolCall { tool_name, .. } => Some(tool_name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(tool_names, vec!["read_file", "write_file"]);

    // Spot-check the shape of one entry per variant that could quietly
    // break: cost carries the USD figure, diagnostics carries the
    // counts, approval carries the decision string.
    for e in &entries {
        match e {
            SessionEntry::Cost { cost_usd, .. } => {
                assert!((cost_usd - 0.0125).abs() < 1e-9);
            }
            SessionEntry::Diagnostics {
                error_count,
                warning_count,
                ..
            } => {
                assert_eq!((*error_count, *warning_count), (1, 2));
            }
            SessionEntry::Approval { decision, .. } => {
                assert_eq!(decision, "approve");
            }
            SessionEntry::PolicyDecision {
                outcome, source, ..
            } => {
                assert_eq!(outcome, "ask");
                assert_eq!(source, "preset");
            }
            SessionEntry::MemoryWrite { channel, tags, .. } => {
                assert_eq!(channel, "extraction");
                assert_eq!(tags, &vec!["auto-fact".to_string(), "rust".to_string()]);
            }
            SessionEntry::ModelFallback { from, to, .. } => {
                assert!(from.contains("qwen2.5-coder"));
                assert!(to.contains("claude-sonnet"));
            }
            SessionEntry::ToolCall { .. } => {}
            SessionEntry::JevDecision { .. } => {}
        }
    }
}

#[test]
fn a_corrupt_line_is_still_a_hard_error() {
    // The forward-compat skip is for *valid JSON with an unknown kind*.
    // A truncated or malformed line is still a real error — the log is
    // machine-written, and a partial line means the file is corrupt.
    // Skipping it would hide a real truncation.
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("corrupt.jsonl");
    std::fs::write(&path, "{\"kind\":\"tool_call\",\"timestamp_ms\":\n").unwrap();

    let err = read_session(&path).expect_err("corrupt line must error");
    let msg = err.to_string();
    assert!(
        msg.contains("line 1"),
        "error should name the line, got: {msg}",
    );
}

#[test]
fn only_the_unknown_kind_is_skipped() {
    // The unknown line sits in the middle; the entries around it must
    // both survive, in order. A regression that aborted at the first
    // unknown line would drop the trailing entry.
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("mixed.jsonl");
    std::fs::write(
        &path,
        concat!(
            "{\"kind\":\"tool_call\",\"timestamp_ms\":1,\"holder\":\"s\",",
            "\"tool_name\":\"first\",\"arguments\":{},\"duration_ms\":1,",
            "\"result\":{\"success\":{}}}\n",
            "{\"kind\":\"never_seen\",\"value\":1}\n",
            "{\"kind\":\"tool_call\",\"timestamp_ms\":2,\"holder\":\"s\",",
            "\"tool_name\":\"second\",\"arguments\":{},\"duration_ms\":2,",
            "\"result\":{\"success\":{}}}\n",
        ),
    )
    .unwrap();

    let entries = read_session(&path).unwrap();
    assert_eq!(entries.len(), 2);
    let names: Vec<&str> = entries
        .iter()
        .filter_map(|e| match e {
            SessionEntry::ToolCall { tool_name, .. } => Some(tool_name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(names, vec!["first", "second"]);
}
