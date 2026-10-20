//! Source-level guard: kod-core-state's trace result preview uses the
//! `truncate_with_ellipsis` helper, not a byte-length guard followed
//! by a `truncate_chars`-plus-format pattern.
//!
//! Round 1B of the workspace byte/char-mismatch migration (session 18).
//! The pre-fix shape:
//!
//!     if s.len() > 240 {
//!         format!("{}…", truncate_chars(&s, 240))
//!     } else {
//!         s
//!     }
//!
//! On CJK input whose char count is at the cap but whose byte count
//! exceeds it, the guard fires, truncate_chars returns the string
//! unchanged, and the ellipsis is appended to a string that was never
//! truncated. The helper (kod_types::strutil::truncate_with_ellipsis)
//! is unit-tested in kod-types; this file pins the site's use of it.

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
fn trace_tool_result_preview_uses_the_helper() {
    let src = read("crates/kod-core-state/src/trace.rs");
    assert!(
        !src.contains("if s.len() > 240 {"),
        "trace.rs must not guard a truncate_chars call with a byte-length check; \
         use kod_types::strutil::truncate_with_ellipsis instead",
    );
    assert!(
        src.contains("truncate_with_ellipsis(&s, 240"),
        "trace.rs must use truncate_with_ellipsis(&s, 240, ...) at the preview site",
    );
}
