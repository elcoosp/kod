//! CJK-length bug in `format_call_brief` / `format_tool_header`
//! (session 18, adversarial-input arc).
//!
//! All five truncation sites in `render.rs` use a **byte-length
//! guard** (`s.len() > N`) before calling `truncate_chars(s, N)`.
//! Post-P2-fix, `truncate_chars` is a **char cap** — on CJK input,
//! `s.chars().count() == N` while `s.len() == 3N`, so the guard
//! fires and `truncate_chars` returns the string *unchanged*. The
//! caller then appends an ellipsis anyway, producing a spurious one
//! on a string that was never truncated.
//!
//! This is the exact shape fixed at `router.rs:1313` in `54c2218`;
//! the same sites in `render.rs` were missed.
//!
//! Each test uses an input whose **char count is exactly at the cap**
//! (so no truncation should happen) but whose **byte count exceeds
//! it** (so the pre-fix guard fires). The assertion: no spurious
//! ellipsis, and the full input survives.

use kod_core::engine::{format_call_brief, format_tool_header};
use serde_json::json;

fn cjk(n: usize) -> String {
    "你".repeat(n)
}

#[test]
fn command_branch_does_not_add_a_spurious_ellipsis_at_the_char_cap() {
    let cmd = cjk(100);
    let brief = format_call_brief("execute_command", &json!({ "command": cmd }));
    assert!(
        brief.contains(&cmd),
        "the full 100-char command must survive; got {brief:?}",
    );
    assert!(
        !brief.ends_with('…'),
        "a command at the char cap must not carry a spurious ellipsis; \
         got {brief:?}",
    );
}

#[test]
fn key_value_branch_does_not_add_a_spurious_ellipsis_at_the_char_cap() {
    let path = cjk(80);
    let brief = format_call_brief("read_file", &json!({ "path": path }));
    assert!(
        brief.contains(&path),
        "the full 80-char path must survive; got {brief:?}",
    );
    assert!(
        !brief.ends_with('…'),
        "a path at the char cap must not carry a spurious ellipsis; \
         got {brief:?}",
    );
}

#[test]
fn fallback_branch_does_not_add_a_spurious_ellipsis_at_the_char_cap() {
    // Raw JSON `{"k":"<72 CJK>"}` is exactly 80 chars and 224 bytes.
    let args = json!({ "k": cjk(72) });
    let raw = args.to_string();
    assert_eq!(raw.chars().count(), 80, "fixture must be 80 chars");
    assert!(raw.len() > 80, "fixture must be >80 bytes");
    let brief = format_call_brief("unknown_tool", &args);
    assert!(
        !brief.ends_with("..."),
        "an 80-char raw JSON must not carry a spurious ellipsis; \
         got {brief:?}",
    );
}

#[test]
fn header_branch_does_not_add_a_spurious_ellipsis_at_the_char_cap() {
    let path = cjk(60);
    let header = format_tool_header("read_file", &json!({ "path": path }));
    assert!(
        header.contains(&path),
        "the full 60-char path must survive in the header; got {header:?}",
    );
    assert!(
        !header.ends_with('…'),
        "a header value at the char cap must not carry a spurious \
         ellipsis; got {header:?}",
    );
}

#[test]
fn intent_branch_does_not_add_a_spurious_ellipsis_at_the_char_cap() {
    // Site 5: `tool_intent` in render.rs (the only remaining
    // byte-guard after the previous commit — which claimed five
    // sites fixed but only replaced four; this test closes the gap
    // and the commit message records the over-claim).
    let intent = cjk(80);
    let brief = format_call_brief("read_file", &json!({ "path": "x", "intent": intent }));
    assert!(
        brief.ends_with(&intent),
        "the full 80-char intent must survive after the em-dash; \
         got {brief:?}",
    );
    assert!(
        !brief.ends_with('…'),
        "an intent at the char cap must not carry a spurious \
         ellipsis; got {brief:?}",
    );
}
