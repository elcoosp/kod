//! Source-level guard: kod-tools' web-fetch error-body preview uses
//! `truncate_with_ellipsis`, not a byte-length guard followed by a
//! `truncate_chars`-plus-format pattern.
//!
//! Round 2C of the workspace byte/char-mismatch migration (session 18).

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
fn web_error_body_preview_uses_the_helper() {
    let src = read("crates/kod-tools/src/web.rs");
    assert!(
        !src.contains("if body.len() > 300 {"),
        "web.rs must not guard a truncate_chars call with a byte-length check",
    );
    assert!(
        src.contains("truncate_with_ellipsis(&body, 300"),
        "web.rs must use truncate_with_ellipsis(&body, 300, ...) at the preview site",
    );
}
