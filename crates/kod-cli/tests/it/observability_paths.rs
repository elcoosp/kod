//! Path-injected observability commands.
//!
//! `observability.rs` is 588 lines at 13.1%. Most of its functions
//! call `KodConfig::load_default()` (reads `~/.kod`), but four take
//! their file path as an argument:
//!
//! - `run_plan_show(Some(path))`
//! - `run_decisions_show(Some(path), limit)`
//! - `run_trace_list(limit, Some(path))`
//! - `run_trace_show(id, Some(path))`
//!
//! None of those four reads `~/.kod` or builds an engine, so they
//! are testable with a `tempfile::TempDir` and no seam. The paths
//! below exercise the empty / missing case: `StateStore::load`
//! returns an empty state on a missing file, `run_trace_list`
//! short-circuits on `!path.exists()`, and `run_trace_show` finds
//! nothing in an empty trace list. All four return `Ok(())` — none
//! is an error, and that is the contract: "no data yet" is the
//! normal first-run case, not a failure.
use kod_cli::commands::observability::{
    run_decisions_show, run_plan_show, run_trace_list, run_trace_show,
};

/// `run_plan_show` on a path that does not exist. `StateStore::load`
/// returns an empty `EngineState`, so the function prints "No plan
/// recorded" and returns Ok.
#[tokio::test]
async fn plan_show_with_no_state_file_is_ok() {
    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("state.json");
    assert!(!path.exists());
    run_plan_show(Some(path))
        .await
        .expect("missing state file is the normal first-run case, not an error");
}

/// `run_decisions_show` on a path that does not exist.
#[tokio::test]
async fn decisions_show_with_no_state_file_is_ok() {
    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("state.json");
    assert!(!path.exists());
    run_decisions_show(Some(path), 10)
        .await
        .expect("missing state file is not an error");
}

/// `run_trace_list` on a path that does not exist. The function
/// prints a hint about `KOD_SESSION_LOG` and returns Ok.
#[tokio::test]
async fn trace_list_on_missing_file_is_ok() {
    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("turns.jsonl");
    assert!(!path.exists());
    run_trace_list(10, Some(path))
        .await
        .expect("missing trace file is not an error");
}

/// `run_trace_show` with an unknown id on an empty trace file.
#[tokio::test]
async fn trace_show_with_unknown_id_is_ok() {
    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("turns.jsonl");
    std::fs::write(&path, "").unwrap();
    run_trace_show(1, Some(path))
        .await
        .expect("unknown id is not an error");
}

/// `run_plan_show` on a state file that contains an empty object.
/// `StateStore::load` parses it as an empty state, and the plan
/// lookup falls through to the same "no plan recorded" branch.
#[tokio::test]
async fn plan_show_with_empty_state_object_is_ok() {
    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("state.json");
    std::fs::write(&path, "{}").unwrap();
    run_plan_show(Some(path))
        .await
        .expect("empty state object is not an error");
}

/// `run_decisions_show` on a state file with an empty object.
#[tokio::test]
async fn decisions_show_with_empty_state_object_is_ok() {
    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("state.json");
    std::fs::write(&path, "{}").unwrap();
    run_decisions_show(Some(path), 10)
        .await
        .expect("empty state object is not an error");
}
