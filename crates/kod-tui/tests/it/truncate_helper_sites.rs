//! Source-level guard: kod-tui's three display-preview sites in
//! `main_loop.rs` use `truncate_with_ellipsis`, not a byte-length
//! guard followed by a `truncate_chars`-plus-format pattern.
//!
//! Round 3 of the workspace byte/char-mismatch migration (session 18).
//! The three sites:
//!
//!   - duration line — `d`, 100 chars
//!   - session system prompt — `s`, 300 chars
//!   - system-prompt override reply — `rest`, 80 chars
//!
//! All three had the pre-fix shape; on CJK input at the char cap, the
//! guard fired, truncate_chars returned the string unchanged, and the
//! caller appended an ellipsis to a string that was never truncated.
//!
//! The helper is unit-tested in kod-types. This file pins the three
//! sites' adoption of it.

fn read(rel: &str) -> String {
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    std::fs::read_to_string(root.join(rel)).unwrap_or_else(|e| panic!("read {rel}: {e}"))
}

#[test]
fn duration_line_preview_uses_the_helper() {
    let src = read("crates/kod-tui/src/main_loop.rs");
    assert!(
        !src.contains("if d.len() > 100 {"),
        "main_loop.rs duration site must not use a byte-length guard",
    );
    assert!(
        src.contains("truncate_with_ellipsis(d, 100"),
        "duration site must call truncate_with_ellipsis(d, 100, ...)",
    );
}

#[test]
fn session_system_prompt_preview_uses_the_helper() {
    let src = read("crates/kod-tui/src/main_loop.rs");
    assert!(
        !src.contains("if s.len() > 300 {"),
        "main_loop.rs session-system-prompt site must not use a byte-length guard",
    );
    assert!(
        src.contains("truncate_with_ellipsis(s, 300"),
        "session-system-prompt site must call truncate_with_ellipsis(s, 300, ...)",
    );
}

#[test]
fn system_override_reply_preview_uses_the_helper() {
    let src = read("crates/kod-tui/src/main_loop.rs");
    assert!(
        !src.contains("if rest.len() > 80 {"),
        "main_loop.rs override-reply site must not use a byte-length guard",
    );
    assert!(
        src.contains("truncate_with_ellipsis(rest, 80"),
        "override-reply site must call truncate_with_ellipsis(rest, 80, ...)",
    );
}
