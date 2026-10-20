//! Source-level guard: kod-cli's display-preview sites use the
//! `truncate_with_ellipsis` helper, not a byte-length guard followed
//! by a `truncate_chars`-plus-format pattern.
//!
//! Round 1 of the workspace migration (session 18). Every one of the
//! nine sites in the byte/char-mismatch class was the same shape:
//!
//!     let x = if s.len() > N {
//!         format!("{ellipsis}", truncate_chars(s, N))
//!     } else {
//!         owned
//!     };
//!
//! On CJK input whose char count is at the cap but whose byte count
//! exceeds it, the guard fires, truncate_chars returns the string
//! unchanged, and the ellipsis is appended to a string that was
//! never truncated.
//!
//! The helper (kod_types::strutil::truncate_with_ellipsis) is
//! unit-tested in kod-types. This file pins that kod-cli's two sites
//! call it, so a future refactor cannot reintroduce the old shape
//! here without failing.

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
fn admin_body_preview_uses_the_helper() {
    let src = read("crates/kod-cli/src/commands/admin.rs");
    assert!(
        !src.contains("if body.len() > 300 {"),
        "admin.rs must not guard a truncate_chars call with a byte-length check; \
         use kod_types::strutil::truncate_with_ellipsis instead",
    );
    assert!(
        src.contains("kod_types::strutil::truncate_with_ellipsis(&body, 300"),
        "admin.rs must use truncate_with_ellipsis(&body, 300, ...) at the preview site",
    );
}

#[test]
fn chat_system_prompt_preview_uses_the_helper() {
    let src = read("crates/kod-cli/src/commands/chat.rs");
    assert!(
        !src.contains("if sys.len() > 120 {"),
        "chat.rs must not guard a truncate_chars call with a byte-length check; \
         use kod_types::strutil::truncate_with_ellipsis instead",
    );
    assert!(
        src.contains("kod_types::strutil::truncate_with_ellipsis(sys, 120"),
        "chat.rs must use truncate_with_ellipsis(sys, 120, ...) at the preview site",
    );
}
