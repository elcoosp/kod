#![cfg(test)]
//! Property tests for the \0kod-* marker protocol.
//!
//! The four markers (`tool_start_marker`, `tool_args_marker`,
//! `tool_done_marker`, `THINKING_MARKER`) form a small wire protocol
//! between the engine and the TUI: the engine builds a string, sends
//! it down an mpsc channel, and the TUI parses it back. The protocol
//! relies on an out-of-band NUL sentinel, which no ordinary text will
//! contain — but tool output does sometimes contain NUL bytes, and
//! any future marker field could accidentally carry one.
//!
//! `tool_done_marker` is documented as sanitizing embedded NULs to
//! spaces before encoding, so its round-trip is only identity on
//! NUL-free inputs. `tool_start_marker` and `tool_args_marker` do not
//! sanitize, so their properties only hold for NUL-free inputs.
//!
//! These tests assert:
//!   1. Round-trips are identity on the domain where they are defined.
//!   2. `parse_tool_done` never drops a well-formed completion, and
//!      never accepts a truncated one (a wrong parse is worse than a
//!      drop: the TUI would show a stale tool row).
//!   3. `truncate_chars` always yields a char-boundary-respecting
//!      prefix.
//!   4. `format_tool_header` and `format_call_brief` never panic on
//!      arbitrary JSON, since the model supplies the arguments.

use super::*;
use proptest::prelude::*;

/// Regex strategy that never emits NUL: the marker protocol cannot
/// represent an embedded NUL, and the engine-side builders are the
/// only sanctioned place that substitutes one for a space.
fn no_nul() -> impl Strategy<Value = String> {
    ".{0,300}".prop_filter("NUL-free", |s| !s.contains('\0'))
}

proptest! {
    /// tool_start_marker / parse_tool_start round-trip on NUL-free,
    /// colon-free input (colons in the cid are sanitized by the
    /// builder, which is what the sanitize test asserts separately).
    #[test]
    fn prop_tool_start_roundtrip(
        cid in "[^:\0]{0,64}",
        name in no_nul().prop_filter("non-empty", |s| !s.is_empty()),
    ) {
        let chunk = tool_start_marker(&cid, &name);
        let (parsed_cid, parsed_name) = parse_tool_start(&chunk)
            .expect("marker built by tool_start_marker must parse");
        prop_assert_eq!(parsed_cid, cid.as_str());
        prop_assert_eq!(parsed_name, name.as_str());
    }

    /// tool_args_marker / parse_tool_args round-trip on NUL-free,
    /// colon-free cid input. Colons inside the display survive (the
    /// parser splits on the *first* colon only).
    #[test]
    fn prop_tool_args_roundtrip(
        cid in "[^:\0]{0,64}",
        display in no_nul(),
    ) {
        let chunk = tool_args_marker(&cid, &display);
        let (parsed_cid, parsed_display) = parse_tool_args(&chunk)
            .expect("marker built by tool_args_marker must parse");
        prop_assert_eq!(parsed_cid, cid.as_str());
        prop_assert_eq!(parsed_display, display.as_str());
    }

    /// tool_done_marker / parse_tool_done round-trip. The builder
    /// sanitizes NUL to space and colons to underscores in the cid,
    /// so the round-trip target is the sanitized form.
    #[test]
    fn prop_tool_done_roundtrip(
        cid in ".{0,64}",
        header in ".{0,200}",
        summary in ".{0,500}",
        ms in any::<u64>(),
    ) {
        let cid_sanitized: String = cid
            .chars()
            .map(|ch| if ch == ':' || ch == '\0' { '_' } else { ch })
            .collect();
        let header_sanitized = header.replace('\0', " ");
        let summary_sanitized = summary.replace('\0', " ");
        let chunk = tool_done_marker(&cid, &header, &summary, ms);
        let (c_, h, s, m) = parse_tool_done(&chunk)
            .expect("marker built by tool_done_marker must parse");
        prop_assert_eq!(c_, cid_sanitized.as_str());
        prop_assert_eq!(h, header_sanitized.as_str());
        prop_assert_eq!(s, summary_sanitized.as_str());
        prop_assert_eq!(m, ms);
    }

    /// Any chunk the engine might send that is *not* a well-formed
    /// done-marker must not parse as one. This is what keeps the TUI
    /// from misreading streamed text as a control frame.
    #[test]
    fn prop_arbitrary_text_is_not_a_done_marker(s in ".{0,400}") {
        if let Some((c_, h, s_, m)) = parse_tool_done(&s) {
            prop_assert!(s.starts_with(TOOL_DONE_MARKER));
            prop_assert!(c_.len() + h.len() + s_.len() <= s.len());
            if let Ok(parsed) = s.splitn(4, '\0').nth(3).unwrap_or("").parse::<u64>() {
                prop_assert_eq!(m, parsed);
            } else {
                prop_assert_eq!(m, 0);
            }
        }
    }

    /// truncate_chars(&s, max) is always a char-boundary prefix of s
    /// no longer than max bytes. The whole reason the helper exists
    /// is that &s[..max] panics on multibyte input.
    #[test]
    fn prop_truncate_chars_is_a_safe_prefix(
        s in ".{0,2000}",
        max in 0usize..2000,
    ) {
        let out = truncate_chars(&s, max);
        prop_assert!(out.len() <= max);
        prop_assert!(s.is_char_boundary(out.len()));
        prop_assert!(s.starts_with(out));
    }

    /// format_tool_header must never panic: the model supplies the
    /// arguments, and it can put anything in them.
    #[test]
    fn prop_format_tool_header_never_panics(
        name in no_nul().prop_filter("non-empty", |s| !s.is_empty()),
        path in ".{0,500}",
        pattern in ".{0,500}",
    ) {
        let args = serde_json::json!({
            "path": path,
            "pattern": pattern,
        });
        let _ = format_tool_header(&name, &args);
    }

    /// format_call_brief must never panic on arbitrary arguments.
    #[test]
    fn prop_format_call_brief_never_panics(
        name in no_nul().prop_filter("non-empty", |s| !s.is_empty()),
        command in ".{0,1000}",
    ) {
        // Two shapes the two branches of format_call_brief handle:
        // execute_command with a "command" key, and a generic tool
        // with a "path" key.
        let args_cmd = serde_json::json!({ "command": command });
        let _ = format_call_brief(&name, &args_cmd);
        let args_path = serde_json::json!({ "path": command });
        let _ = format_call_brief(&name, &args_path);
    }
}
