#![cfg(test)]
//! Tests for the auto-check injection in `run_tool_calls`.
//!
//! The feature is subtle: after a write_file succeeds, the engine
//! runs the project's compiler and appends its diagnostics to the
//! prompt the model sees on the next round. These tests drive a
//! real tool round against a real tempfile workspace, so a
//! regression in the injection shows up here rather than in
//! production.

use super::*;
use kod_types::ToolCall;
use tempfile::TempDir;

/// A clean project: the write is followed by "Auto-check … is
/// clean." in the prompt block.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_auto_check_clean_project() {
    let tmp = TempDir::new().unwrap();
    // Minimal Cargo project so CheckTool detects "cargo" and runs
    // `cargo check` — but the file has no errors, so the diagnostics
    // list is empty.
    std::fs::write(
        tmp.path().join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(tmp.path().join("src/lib.rs"), "pub fn ok() {}\n").unwrap();

    let db_path = tmp.path().join("test.redb");
    let cfg = RouterConfig {
        embedder: None,
        skill_threshold: 0.3,
        context_window: 8192,
        short_term_capacity: 100,
        working_dir: tmp.path().to_path_buf(),
        enable_memory: false,
        max_skills_per_query: 3,
    };
    let engine = KodEngine::new(cfg, db_path).unwrap();
    engine.set_auto_check(true);
    engine.start().await.unwrap();

    let calls = vec![ToolCall {
        id: None,
        tool_name: "write_file".to_string(),
        arguments: serde_json::json!({
            "path": "src/lib.rs",
            "content": "pub fn ok() {}\n// added a harmless comment\n"
        }),
    }];
    let round = engine.run_tool_calls(&calls, "test", None).await;
    assert_eq!(round.results.len(), 1);

    // The write succeeded (auto-check runs only on success).
    assert!(matches!(&round.results[0], ToolResult::Success(_)));

    // The prompt block should carry the auto-check section. We do
    // not assert "clean" text exactly because the cargo output
    // format may evolve; the presence of `## Auto-check` is the
    // contract.
    assert!(
        round.prompt_block.contains("## Auto-check"),
        "clean auto-check block missing: {}",
        round.prompt_block
    );
    assert!(
        round.prompt_block.contains("is clean"),
        "clean auto-check should say so: {}",
        round.prompt_block
    );
}

/// Auto-check is disabled by default: the block does not appear
/// even when a write succeeds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_auto_check_disabled_by_default() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(
        tmp.path().join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(tmp.path().join("src/lib.rs"), "pub fn ok() {}\n").unwrap();

    let db_path = tmp.path().join("test.redb");
    let cfg = RouterConfig {
        embedder: None,
        skill_threshold: 0.3,
        context_window: 8192,
        short_term_capacity: 100,
        working_dir: tmp.path().to_path_buf(),
        enable_memory: false,
        max_skills_per_query: 3,
    };
    let engine = KodEngine::new(cfg, db_path).unwrap();
    // Intentionally NOT calling set_auto_check(true).
    engine.start().await.unwrap();

    let calls = vec![ToolCall {
        id: None,
        tool_name: "write_file".to_string(),
        arguments: serde_json::json!({
            "path": "src/lib.rs",
            "content": "pub fn ok() {}\n"
        }),
    }];
    let round = engine.run_tool_calls(&calls, "test", None).await;
    assert!(
        !round.prompt_block.contains("## Auto-check"),
        "auto-check must not run when disabled: {}",
        round.prompt_block
    );
}

/// A round that writes two files must fall through to the
/// compiler, which sees both. The per-file LSP path would answer
/// for only one of them.
///
/// This test works without rust-analyzer on PATH: the multi-file
/// shape makes `lsp_eligible` false regardless, so the compiler
/// path runs on every machine. What it proves is that the
/// multi-file case never short-circuits through LSP.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_auto_check_multi_file_round_uses_compiler() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(
        tmp.path().join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(tmp.path().join("src/lib.rs"), "pub mod extra;\n").unwrap();
    std::fs::write(
        tmp.path().join("src/extra.rs"),
        "pub fn bad() -> u32 { 42 }\n",
    )
    .unwrap();

    let db_path = tmp.path().join("test.redb");
    let cfg = RouterConfig {
        embedder: None,
        skill_threshold: 0.3,
        context_window: 8192,
        short_term_capacity: 100,
        working_dir: tmp.path().to_path_buf(),
        enable_memory: false,
        max_skills_per_query: 3,
    };
    let engine = KodEngine::new(cfg, db_path).unwrap();
    engine.set_auto_check(true);
    engine.start().await.unwrap();

    // The round writes both files: lib.rs unchanged in spirit,
    // extra.rs introduces a type error. Only the compiler sees
    // both.
    let calls = vec![
        ToolCall {
            id: None,
            tool_name: "write_file".to_string(),
            arguments: serde_json::json!({
                "path": "src/lib.rs",
                "content": "pub mod extra;\n// harmless comment\n"
            }),
        },
        ToolCall {
            id: None,
            tool_name: "write_file".to_string(),
            arguments: serde_json::json!({
                "path": "src/extra.rs",
                "content": "pub fn bad() -> u32 { \"not a number\" }\n"
            }),
        },
    ];
    let round = engine.run_tool_calls(&calls, "test", None).await;
    assert_eq!(round.results.len(), 2);

    assert!(
        round.prompt_block.contains("## Auto-check"),
        "multi-file round should trigger auto-check: {}",
        round.prompt_block
    );
    assert!(
        round.prompt_block.contains("extra.rs"),
        "the compiler should name the file with the error: {}",
        round.prompt_block
    );
}

/// Auto-check with a non-write tool call must not trigger. The
/// engine should not run `cargo check` for a `read_file`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_auto_check_skips_read_only_round() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(
        tmp.path().join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(tmp.path().join("src/lib.rs"), "pub fn ok() {}\n").unwrap();

    let db_path = tmp.path().join("test.redb");
    let cfg = RouterConfig {
        embedder: None,
        skill_threshold: 0.3,
        context_window: 8192,
        short_term_capacity: 100,
        working_dir: tmp.path().to_path_buf(),
        enable_memory: false,
        max_skills_per_query: 3,
    };
    let engine = KodEngine::new(cfg, db_path).unwrap();
    engine.set_auto_check(true);
    engine.start().await.unwrap();

    let calls = vec![ToolCall {
        id: None,
        tool_name: "read_file".to_string(),
        arguments: serde_json::json!({ "path": "src/lib.rs" }),
    }];
    let round = engine.run_tool_calls(&calls, "test", None).await;
    assert!(
        !round.prompt_block.contains("## Auto-check"),
        "read-only round must not trigger auto-check: {}",
        round.prompt_block
    );
}
/// The compiler auto-check's decision table, tested directly.
///
/// The pre-fix version of this test drove `run_tool_calls`, which
/// runs a real `cargo check` twice (baseline, then current). On a
/// contended CI runner the nested check does not complete,
/// `refresh_check_baseline` silently leaves the baseline `None`, and
/// the test failed on "baseline should have captured the pre-existing
/// error" — asserting on a subprocess succeeding rather than on the
/// logic it meant to pin. The decision table now lives in
/// `render_compiler_auto_check` and is tested here with no subprocess.
#[test]
fn compiler_auto_check_reports_only_new_diagnostics() {
    use kod_tools::check::Diagnostic;

    let pre_existing = Diagnostic {
        file: "src/existing.rs".to_string(),
        line: 1,
        column: 20,
        severity: "error".to_string(),
        code: Some("E0308".to_string()),
        message: "mismatched types".to_string(),
    };
    let baseline = vec![pre_existing.clone()];

    // The write touched a clean file: the compiler sees the same
    // pre-existing error, nothing new.
    let current = vec![pre_existing.clone()];
    let new: Vec<&Diagnostic> = Vec::new();

    let mut block = String::new();
    crate::engine::render::render_compiler_auto_check(
        &mut block,
        "cargo check",
        &current,
        Some(&baseline),
        &new,
    );
    assert!(
        block.contains("no new errors"),
        "a write that introduces nothing must say so: {block}",
    );
    assert!(
        !block.contains("NEW diagnostic"),
        "the pre-existing error must not be reported as new: {block}",
    );
}

/// The complement: a genuinely new error *is* reported.
#[test]
fn compiler_auto_check_reports_a_genuinely_new_diagnostic() {
    use kod_tools::check::Diagnostic;

    let pre_existing = Diagnostic {
        file: "src/existing.rs".to_string(),
        line: 1,
        column: 20,
        severity: "error".to_string(),
        code: Some("E0308".to_string()),
        message: "mismatched types".to_string(),
    };
    let introduced = Diagnostic {
        file: "src/touched.rs".to_string(),
        line: 2,
        column: 5,
        severity: "error".to_string(),
        code: Some("E0425".to_string()),
        message: "cannot find value".to_string(),
    };
    let baseline = vec![pre_existing.clone()];
    let current = vec![pre_existing, introduced.clone()];
    let new = vec![&introduced];

    let mut block = String::new();
    crate::engine::render::render_compiler_auto_check(
        &mut block,
        "cargo check",
        &current,
        Some(&baseline),
        &new,
    );
    assert!(
        block.contains("NEW diagnostic"),
        "a genuinely new error must be reported: {block}",
    );
    assert!(
        block.contains("touched.rs"),
        "the new error's file must appear: {block}",
    );
}

/// No baseline captured: every diagnostic is shown with the caveat,
/// because the engine cannot tell new from pre-existing.
#[test]
fn compiler_auto_check_without_a_baseline_shows_all_with_a_caveat() {
    use kod_tools::check::Diagnostic;

    let d = Diagnostic {
        file: "src/a.rs".to_string(),
        line: 1,
        column: 1,
        severity: "error".to_string(),
        code: None,
        message: "boom".to_string(),
    };
    let current = vec![d.clone()];
    let new = vec![&d];

    let mut block = String::new();
    crate::engine::render::render_compiler_auto_check(
        &mut block,
        "cargo check",
        &current,
        None,
        &new,
    );
    assert!(
        block.contains("No baseline was captured"),
        "a missing baseline must be stated: {block}",
    );
}

/// A clean project with a baseline says "is clean".
#[test]
fn compiler_auto_check_clean_project_says_clean() {
    let mut block = String::new();
    crate::engine::render::render_compiler_auto_check(
        &mut block,
        "cargo check",
        &[],
        Some(&[]),
        &[],
    );
    assert!(block.contains("is clean"), "got: {block}");
}
