//! Source-level guard: kod-provider-anthropic's error-snippet preview
//! uses `truncate_with_ellipsis`, not a byte-length guard followed by a
//! `truncate_chars`-plus-format pattern.
//!
//! Round 2B of the workspace byte/char-mismatch migration (session 18).

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
fn anthropic_error_snippet_uses_the_helper() {
    let src = read("crates/kod-provider-anthropic/src/provider.rs");
    assert!(
        !src.contains("if text.len() > 400 {"),
        "provider.rs must not guard a truncate_chars call with a byte-length check",
    );
    assert!(
        src.contains("truncate_with_ellipsis(&text, 400"),
        "provider.rs must use truncate_with_ellipsis(&text, 400, ...) at the snippet site",
    );
}
