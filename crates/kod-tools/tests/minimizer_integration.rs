//! End-to-end: `execute_command` minimizes stdout through the
//! internal-URL-backed minimizer + artifact store (delta §5).
//!
//! Proves the whole path: a real `git status` runs through a real
//! `ExecuteCommandTool` with a `ToolContext` carrying a
//! `Minimizer::with_builtins()` and an `ArtifactStoreHook` that
//! writes to a real `ArtifactHandler`. The result must:
//!
//! * carry `stdout_minimized_by = "git-status"` (the def matched);
//! * carry an `artifact://` URL in `stdout_artifact`;
//! * carry a `stdout` field with the ANSI-stripped output;
//! * round-trip: a `ReadFileTool` with the same router resolves the
//!   artifact URL and returns the raw text.
//!
//! A second test proves the negative: a context without the
//! minimizer returns the raw output and null fields — the shape is
//! unchanged for every embedder and unit test that did not opt in.

use kod_tools::context::ArtifactStoreHook;
use kod_tools::internal_url::{
    ArtifactHandler, ProtocolHandler, ProtocolRouter, ResolveContext,
};
use kod_tools::{
    ExecuteCommandTool, ReadFileTool, Tool, ToolContext, ToolResult,
};
use std::sync::Arc;

/// Make a temp git repo with a known `git status` shape. Returns
/// the `TempDir` so the caller keeps it alive for the test's
/// duration.
fn make_git_repo() -> tempfile::TempDir {
    let tmp = tempfile::TempDir::new().unwrap();
    let run = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(tmp.path())
            .output()
            .expect("git available");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr),
        );
    };
    run(&["init", "-q", "-b", "main"]);
    run(&["config", "user.email", "test@example.com"]);
    run(&["config", "user.name", "Test"]);
    std::fs::write(tmp.path().join("tracked.rs"), "fn main() {}\n").unwrap();
    run(&["add", "tracked.rs"]);
    run(&["commit", "-q", "-m", "initial"]);
    // Make it dirty so `git status` has content to report.
    std::fs::write(tmp.path().join("tracked.rs"), "fn main() { }\n").unwrap();
    std::fs::write(tmp.path().join("untracked.rs"), "// new\n").unwrap();
    tmp
}

fn make_context_with_minimizer(
    working_dir: &std::path::Path,
    handler: Arc<ArtifactHandler>,
) -> ToolContext {
    let router = ProtocolRouter::new()
        .register(Arc::clone(&handler) as Arc<dyn ProtocolHandler>);
    let store: ArtifactStoreHook = {
        let handler = Arc::clone(&handler);
        ArtifactStoreHook::new(move |id, text, mime| {
            let handler = Arc::clone(&handler);
            Box::pin(async move {
                handler
                    .store(id, text, mime)
                    .await
                    .map_err(|e| kod_error::KodError::InvalidParameters {
                        reason: format!("artifact store: {e}"),
                    })
            })
        })
    };
    let minimizer = Arc::new(kod_tools::kod_minimize::Minimizer::with_builtins());
    ToolContext::new(working_dir)
        .with_permissions(kod_types::ToolPermissions {
            read_files: true,
            write_files: true,
            execute_commands: true,
            ..Default::default()
        })
        .with_locks(Arc::new(kod_tools::PathLockTable::new()), "session")
        .with_protocol_router(router)
        .with_minimizer(minimizer, store)
}

fn make_plain_context(working_dir: &std::path::Path) -> ToolContext {
    ToolContext::new(working_dir)
        .with_permissions(kod_types::ToolPermissions {
            read_files: true,
            write_files: true,
            execute_commands: true,
            ..Default::default()
        })
        .with_locks(Arc::new(kod_tools::PathLockTable::new()), "session")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_status_is_minimized_and_raw_offloads_to_an_artifact() {
    let tmp = make_git_repo();
    let handler = Arc::new(ArtifactHandler::new());
    let ctx = make_context_with_minimizer(tmp.path(), Arc::clone(&handler));

    let tool = ExecuteCommandTool::new();
    let r = tool
        .execute(
            &serde_json::json!({"command": "git status", "timeout_secs": 30}),
            &ctx,
        )
        .await
        .unwrap();

    let ToolResult::Success(v) = r else {
        panic!("expected Success, got {r:?}");
    };
    assert_eq!(
        v["exit_code"].as_i64(),
        Some(0),
        "git status should succeed; result = {v}",
    );
    assert_eq!(
        v["stdout_minimized_by"].as_str(),
        Some("git-status"),
        "the git-status def must match; result = {v}",
    );
    let artifact_url = v["stdout_artifact"]
        .as_str()
        .unwrap_or_else(|| panic!("expected an artifact URL; result = {v}"));
    assert!(
        artifact_url.starts_with("artifact://cmd-"),
        "artifact URL must be the content-hashed cmd-* form; got {artifact_url}",
    );

    // The stdout the model sees must be ANSI-free and contain the
    // branch line — both are what the git-status def promises.
    let stdout = v["stdout"].as_str().unwrap_or_default();
    assert!(
        !stdout.contains('\u{1b}'),
        "minimized stdout must be ANSI-free; got {stdout:?}",
    );
    assert!(
        stdout.contains("On branch main"),
        "minimized stdout must keep the branch line; got {stdout:?}",
    );

    // Round-trip: read the artifact through the same router.
    let reader = ReadFileTool::new();
    let r = reader
        .execute(&serde_json::json!({"path": artifact_url}), &ctx)
        .await
        .unwrap();
    let ToolResult::Success(a) = r else {
        panic!("expected Success reading artifact, got {r:?}");
    };
    assert_eq!(a["source"].as_str(), Some("internal-url"));
    let artifact_text = a["content"].as_str().unwrap_or_default();
    // The artifact holds the *raw* capture. `git status` on a clean
    // checkout prints "On branch main" too, so the branch line is
    // the round-trip marker that survives regardless of the exact
    // dirty-file list.
    assert!(
        artifact_text.contains("On branch main"),
        "artifact must round-trip the raw git status; got {artifact_text:?}",
    );
    assert!(
        artifact_text.len() >= stdout.len(),
        "artifact holds the raw capture, which must be at least as \
         long as the minimized form; artifact={} minimized={}",
        artifact_text.len(),
        stdout.len(),
    );

    // The handler's store is shared: exactly one artifact.
    assert_eq!(
        handler.len().await,
        1,
        "one command produced one artifact",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_minimizer_less_context_returns_the_raw_output_and_null_fields() {
    let tmp = make_git_repo();
    let ctx = make_plain_context(tmp.path());

    let tool = ExecuteCommandTool::new();
    let r = tool
        .execute(
            &serde_json::json!({"command": "git status", "timeout_secs": 30}),
            &ctx,
        )
        .await
        .unwrap();

    let ToolResult::Success(v) = r else {
        panic!("expected Success, got {r:?}");
    };
    assert!(
        v["stdout_minimized_by"].is_null(),
        "no minimizer means no def matched; result = {v}",
    );
    assert!(
        v["stdout_artifact"].is_null(),
        "no minimizer means nothing offloaded; result = {v}",
    );
    // The stdout is still there and it is the raw output.
    assert!(
        v["stdout"].as_str().unwrap_or_default().contains("On branch main"),
        "raw stdout must be present; result = {v}",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_piped_command_is_not_minimized_even_with_a_minimizer_installed() {
    let tmp = make_git_repo();
    let handler = Arc::new(ArtifactHandler::new());
    let ctx = make_context_with_minimizer(tmp.path(), Arc::clone(&handler));

    let tool = ExecuteCommandTool::new();
    let r = tool
        .execute(
            &serde_json::json!({
                "command": "git status | head -1",
                "timeout_secs": 30,
            }),
            &ctx,
        )
        .await
        .unwrap();

    let ToolResult::Success(v) = r else {
        panic!("expected Success, got {r:?}");
    };
    assert!(
        v["stdout_minimized_by"].is_null(),
        "piped commands are opaque; result = {v}",
    );
    assert!(
        v["stdout_artifact"].is_null(),
        "no rewrite means no offload; result = {v}",
    );
    assert_eq!(
        handler.len().await,
        0,
        "no rewrite means no artifact",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_non_matching_command_is_not_minimized() {
    let tmp = make_git_repo();
    let handler = Arc::new(ArtifactHandler::new());
    let ctx = make_context_with_minimizer(tmp.path(), Arc::clone(&handler));

    let tool = ExecuteCommandTool::new();
    let r = tool
        .execute(
            &serde_json::json!({"command": "echo hello", "timeout_secs": 30}),
            &ctx,
        )
        .await
        .unwrap();

    let ToolResult::Success(v) = r else {
        panic!("expected Success, got {r:?}");
    };
    assert!(v["stdout_minimized_by"].is_null(), "result = {v}");
    assert!(v["stdout_artifact"].is_null(), "result = {v}");
    assert_eq!(v["stdout"].as_str(), Some("hello\n"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_same_command_twice_produces_the_same_artifact_id() {
    // The id is a content hash; the store refuses overwrite. A
    // second invocation with identical output must reuse the id
    // (which the store accepts as a no-op) rather than mint a new
    // one.
    let tmp = make_git_repo();
    let handler = Arc::new(ArtifactHandler::new());
    let ctx = make_context_with_minimizer(tmp.path(), Arc::clone(&handler));

    let tool = ExecuteCommandTool::new();
    let params = serde_json::json!({"command": "git status", "timeout_secs": 30});
    let r1 = tool.execute(&params, &ctx).await.unwrap();
    let r2 = tool.execute(&params, &ctx).await.unwrap();

    let ToolResult::Success(v1) = r1 else { panic!("r1") };
    let ToolResult::Success(v2) = r2 else { panic!("r2") };

    let u1 = v1["stdout_artifact"].as_str().unwrap();
    let u2 = v2["stdout_artifact"].as_str().unwrap();
    assert_eq!(u1, u2, "same command + same output = same id");
    // The store's overwrite refusal means only one artifact exists.
    assert_eq!(handler.len().await, 1);
}

// The `ResolveContext` type is used by the router, but the test does
// not construct one directly — the tool does. Kept as an unused
// import guard so a future refactor that removes the router from the
// tool still compiles this file.
#[allow(dead_code)]
fn _assert_resolve_context_is_exported() {
    let _ = ResolveContext::new("x", "/tmp");
}
