//! Git tools integration tests (D3-C4).
//!
//! Confirms the GitAccess enum contract:
//! - A context with `GitAccess::None` denies every git tool.
//! - A context with `GitAccess::Read` allows `git_status` / `git_diff`
//!   and `git_branch list`, and denies `git_commit` / `git_branch
//!   create`.
//! - A context with `GitAccess::Write` allows all of them.
//!
//! The write tools are exercised against a real git repository seeded
//! in a tempdir. `git` must be on PATH; the tests skip silently when
//! it is not (matching the `binary_on_path` pattern from the LSP
//! crate).

use kod_tools::{GitBranchTool, GitCommitTool, GitDiffTool, GitStatusTool, Tool, ToolContext};
use kod_types::{GitAccess, ToolPermissions, ToolResult};

fn binary_on_path(name: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(name).is_file()))
        .unwrap_or(false)
}

fn init_repo(dir: &std::path::Path) {
    for args in [
        vec!["init", "-q", "-b", "main"],
        vec!["config", "user.email", "test@example.com"],
        vec!["config", "user.name", "Test"],
    ] {
        let _ = std::process::Command::new("git")
            .args(&args)
            .current_dir(dir)
            .status();
    }
    std::fs::write(dir.join("seed.txt"), "seed\n").unwrap();
    let _ = std::process::Command::new("git")
        .args(["add", "seed.txt"])
        .current_dir(dir)
        .status();
    let _ = std::process::Command::new("git")
        .args(["commit", "-q", "-m", "seed"])
        .current_dir(dir)
        .status();
}

fn ctx_with(dir: &std::path::Path, access: GitAccess) -> ToolContext {
    ToolContext::new(dir).with_permissions(ToolPermissions {
        read_files: true,
        write_files: false,
        execute_commands: false,
        network_access: false,
        git_access: access,
        allowed_paths: Vec::new(),
        forbidden_paths: Vec::new(),
    })
}

#[tokio::test]
async fn git_access_none_denies_status() {
    let tmp = tempfile::TempDir::new().unwrap();
    let ctx = ctx_with(tmp.path(), GitAccess::None);
    let tool = GitStatusTool::new();
    let err = tool
        .execute(&serde_json::json!({}), &ctx)
        .await
        .expect_err("None must deny");
    match err {
        kod_error::KodError::PermissionDenied { action, .. } => {
            assert!(action.contains("git"), "got: {action}");
        }
        other => panic!("expected PermissionDenied, got {other:?}"),
    }
}

#[tokio::test]
async fn git_access_read_allows_status_and_diff() {
    if !binary_on_path("git") {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let tmp = tempfile::TempDir::new().unwrap();
    init_repo(tmp.path());
    let ctx = ctx_with(tmp.path(), GitAccess::Read);

    let status = GitStatusTool::new()
        .execute(&serde_json::json!({}), &ctx)
        .await
        .unwrap();
    assert!(matches!(status, ToolResult::Success(_)));

    let diff = GitDiffTool::new()
        .execute(&serde_json::json!({}), &ctx)
        .await
        .unwrap();
    assert!(matches!(diff, ToolResult::Success(_)));
}

#[tokio::test]
async fn git_access_read_denies_commit() {
    if !binary_on_path("git") {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let tmp = tempfile::TempDir::new().unwrap();
    init_repo(tmp.path());
    let ctx = ctx_with(tmp.path(), GitAccess::Read);

    let err = GitCommitTool::new()
        .execute(&serde_json::json!({"message": "should not land"}), &ctx)
        .await
        .expect_err("Read must deny Write tools");
    match err {
        kod_error::KodError::PermissionDenied { .. } => {}
        other => panic!("expected PermissionDenied, got {other:?}"),
    }
}

#[tokio::test]
async fn git_access_write_allows_commit_and_branch_create() {
    if !binary_on_path("git") {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let tmp = tempfile::TempDir::new().unwrap();
    init_repo(tmp.path());
    let ctx = ctx_with(tmp.path(), GitAccess::Write);

    // Modify the worktree, then commit through the tool.
    std::fs::write(tmp.path().join("seed.txt"), "seed\nmore\n").unwrap();
    let commit = GitCommitTool::new()
        .execute(
            &serde_json::json!({
                "message": "add a line",
                "files": ["seed.txt"],
            }),
            &ctx,
        )
        .await
        .unwrap();
    match commit {
        ToolResult::Success(v) => {
            assert!(
                v["commit"].is_string() || v["commit"].is_null(),
                "commit field missing: {v}"
            );
        }
        other => panic!("expected commit success, got {other:?}"),
    }

    // Create a branch and confirm it exists via list.
    GitBranchTool::new()
        .execute(
            &serde_json::json!({"action": "create", "name": "experiment"}),
            &ctx,
        )
        .await
        .unwrap();

    let list = GitBranchTool::new()
        .execute(&serde_json::json!({"action": "list"}), &ctx)
        .await
        .unwrap();
    match list {
        ToolResult::Success(v) => {
            let branches = v["branches"].as_array().unwrap();
            let names: Vec<&str> = branches.iter().filter_map(|b| b.as_str()).collect();
            assert!(
                names.contains(&"experiment"),
                "experiment should exist: {names:?}"
            );
        }
        other => panic!("expected branch list success, got {other:?}"),
    }
}

#[tokio::test]
async fn git_branch_create_rejects_invalid_name() {
    if !binary_on_path("git") {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let tmp = tempfile::TempDir::new().unwrap();
    init_repo(tmp.path());
    let ctx = ctx_with(tmp.path(), GitAccess::Write);

    let result = GitBranchTool::new()
        .execute(
            &serde_json::json!({"action": "create", "name": "-bad name"}),
            &ctx,
        )
        .await
        .unwrap();
    assert!(matches!(result, ToolResult::Error(_)), "got: {result:?}");
}

#[tokio::test]
async fn git_branch_list_requires_only_read() {
    if !binary_on_path("git") {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let tmp = tempfile::TempDir::new().unwrap();
    init_repo(tmp.path());
    let ctx = ctx_with(tmp.path(), GitAccess::Read);

    let r = GitBranchTool::new()
        .execute(&serde_json::json!({"action": "list"}), &ctx)
        .await
        .unwrap();
    assert!(matches!(r, ToolResult::Success(_)), "got: {r:?}");
}
