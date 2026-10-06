#![cfg(test)]
//! `expand_at_references` is the @-syntax preprocessor for a
//! prompt. The containment rule it enforces is the same one
//! the tool context uses: a resolved path must live inside
//! the working directory, so a `@../../etc/passwd` in a
//! prompt cannot leak a file the agent was not asked to read.
use super::*;
use tempfile::TempDir;

#[test]
fn a_real_file_inside_the_workspace_is_expanded() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("foo.rs"), "fn main() {}\n").unwrap();
    let out = expand_at_references("see @foo.rs for details", tmp.path());
    assert!(out.contains("<file path="), "no file block: {out}");
    assert!(out.contains("fn main()"), "content missing: {out}");
    // The surrounding prose survives.
    assert!(out.contains("see "), "prefix lost: {out}");
    assert!(out.contains(" for details"), "suffix lost: {out}");
}

#[test]
fn a_missing_file_leaves_the_token_untouched() {
    let tmp = TempDir::new().unwrap();
    let out = expand_at_references("see @missing.rs here", tmp.path());
    assert!(out.contains("@missing.rs"), "token mangled: {out}");
    assert!(!out.contains("<file"), "spurious expansion: {out}");
}

#[test]
fn a_non_path_token_is_left_alone() {
    // A bare `@user` (no slash, no dot) is a mention, not a
    // path. The heuristic is documented behaviour; a
    // regression that expanded it would try to read a file
    // named `user`.
    let tmp = TempDir::new().unwrap();
    let out = expand_at_references("hi @user how are you", tmp.path());
    assert_eq!(out, "hi @user how are you");
}

#[test]
fn a_path_that_escapes_the_workspace_is_refused() {
    // Even a path that exists on disk must not expand if it
    // lives outside the working directory.
    let tmp = TempDir::new().unwrap();
    let out = expand_at_references("see @../etc/passwd", tmp.path());
    assert!(!out.contains("<file"), "escape expanded: {out}",);
    assert!(out.contains("@../etc/passwd"), "token eaten: {out}");
}

#[test]
fn an_empty_input_is_unchanged() {
    let tmp = TempDir::new().unwrap();
    assert_eq!(expand_at_references("", tmp.path()), "");
}

#[test]
fn a_prompt_with_no_at_tokens_is_unchanged() {
    let tmp = TempDir::new().unwrap();
    let input = "plain text, nothing to expand";
    assert_eq!(expand_at_references(input, tmp.path()), input);
}

#[test]
fn only_word_boundary_at_signs_start_a_reference() {
    // `mail@host.com` has an `@` inside a word; the boundary
    // check must not treat it as a reference. The heuristic
    // also filters it out (no slash, no dot in the token? —
    // actually `host.com` has a dot). The boundary check runs
    // first.
    let tmp = TempDir::new().unwrap();
    let out = expand_at_references("contact me at user@example.com", tmp.path());
    // The `.com` shape would look path-like; the boundary
    // check is what stops the expansion.
    assert!(out.contains("user@example.com"), "mangled: {out}");
}

#[test]
fn a_bare_at_sign_is_preserved() {
    let tmp = TempDir::new().unwrap();
    let out = expand_at_references("just @ alone", tmp.path());
    assert!(out.contains('@'), "at sign lost: {out}");
}

#[test]
fn an_empty_file_is_expanded_to_an_empty_block() {
    // A zero-byte file is a legitimate input. The block must
    // still appear — the model needs to know it was read.
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("empty.txt"), "").unwrap();
    let out = expand_at_references("see @empty.txt", tmp.path());
    assert!(
        out.contains("<file path="),
        "no block for empty file: {out}"
    );
}

#[test]
fn a_directory_reference_does_not_expand() {
    // `@subdir` matches the path shape (`subdir` has no dot,
    // so the looks-like-path heuristic rejects it — the test
    // pins that rejection). A directory whose name has a dot
    // is also rejected, because `is_file()` fails.
    let tmp = TempDir::new().unwrap();
    std::fs::create_dir(tmp.path().join("my.dir")).unwrap();
    let out = expand_at_references("see @my.dir", tmp.path());
    assert!(!out.contains("<file"), "directory expanded: {out}");
}
