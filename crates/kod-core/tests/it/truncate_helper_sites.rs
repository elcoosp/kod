//! Source-level guard: kod-core's transcript turn preview uses
//! `truncate_with_ellipsis`, not a byte-length guard followed by a
//! `truncate_chars`-plus-format pattern.
//!
//! Round 2A of the workspace byte/char-mismatch migration (session 18).
//! This site carries the multi-character ellipsis `"… [truncated]"`,
//! so the source assertion also pins that form (not just `"…"`).

fn read(rel: &str) -> String {
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    std::fs::read_to_string(root.join(rel))
        .unwrap_or_else(|e| panic!("read {rel}: {e}"))
}

#[test]
fn transcript_turn_preview_uses_the_helper_with_multichar_ellipsis() {
    let src = read("crates/kod-core/src/engine/transcript.rs");
    assert!(
        !src.contains("if text.len() > MAX_TURN_CHARS {"),
        "transcript.rs must not guard a truncate_chars call with a byte-length check",
    );
    assert!(
        src.contains("truncate_with_ellipsis("),
        "transcript.rs must use truncate_with_ellipsis at the preview site",
    );
    assert!(
        src.contains("\"… [truncated]\""),
        "the multi-character ellipsis form must be preserved",
    );
}
