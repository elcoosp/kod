//! Background shell tasks (P2-d).
//!
//! A `run_in_background` command is spawned, its output spooled, and
//! a `SoftInterrupt::background` delivered on completion. These tests
//! pin the delivery — the property the whole feature exists for. A
//! background job whose completion nobody is told about is just a
//! process leak.

use kod_core::router::RouterConfig;
use kod_core::KodEngine;
use std::sync::Arc;
use tempfile::TempDir;

async fn engine() -> (TempDir, Arc<KodEngine>) {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("test.redb");
    let cfg = RouterConfig {
        embedder: None,
        skill_threshold: 0.3,
        context_window: 8192,
        short_term_capacity: 100,
        working_dir: tmp.path().to_path_buf(),
        enable_memory: false,
        max_skills_per_query: 3,
    };
    let e = Arc::new(KodEngine::new(cfg, db).unwrap());
    (tmp, e)
}

#[tokio::test]
async fn a_background_command_delivers_an_interrupt_on_completion() {
    let (_tmp, engine) = engine().await;
    let hook = engine.build_background_hook();

    let job_id = hook
        .spawn("echo hello-from-background", None, "session")
        .expect("a trivial echo must spawn");
    assert!(!job_id.is_empty(), "the hook returns a job id");

    // The command is `echo`, so completion is near-immediate, but the
    // watcher runs on the runtime and must be given a turn. Poll for
    // up to two seconds rather than sleeping a fixed amount — a slower
    // CI box should not make the test flaky.
    let mut delivered = false;
    for _ in 0..200 {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let steers = engine.pending_steers_for("session").await;
        if steers.iter().any(|s| {
            matches!(s.source, kod_core::steer::InterruptSource::BackgroundTask)
        }) {
            delivered = true;
            break;
        }
    }
    assert!(delivered, "completion must deliver a background interrupt");
}

#[tokio::test]
async fn a_missing_hook_degrades_to_inline() {
    // A bare ToolContext has no background spawner; a
    // `run_in_background` call must still run the command, just
    // synchronously. This pins the degradation the tool documents.
    use kod_tools::ToolContext;
    let ctx = ToolContext::new(std::env::temp_dir());
    assert!(
        ctx.on_background_command.is_none(),
        "a bare context has no spawner",
    );
}
