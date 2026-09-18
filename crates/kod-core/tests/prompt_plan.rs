//! `PromptPlan` byte-identity test (design §2 AD-16).
//!
//! The eventual engine migration (AD-01/AD-07) replaces
//! `build_prompt_with_budget(...) -> String` with
//! `build_prompt_plan(...) -> PromptPlan` on every call site. The
//! migration is safe only if the plan renders back to the same bytes the
//! text builder produced — a provider that sees a different prompt is a
//! behaviour change even if no field changed meaning.
//!
//! This file pins the invariant for several representative inputs, plus
//! the cacheable-prefix stability the golden-prefix test asserts on the
//! text form.

use kod_core::router::{PromptPlan, RouterConfig, TaskRouter, TaskType};
use tempfile::TempDir;

fn router_with_one_file(dir: &TempDir) -> TaskRouter {
    std::fs::write(
        dir.path().join("foo.rs"),
        "pub fn foo() -> u32 { 42 }\n",
    )
    .unwrap();
    let db_path = dir.path().join("test.redb");
    let cfg = RouterConfig {
        working_dir: dir.path().to_path_buf(),
        enable_memory: false,
        ..RouterConfig::default()
    };
    TaskRouter::new(cfg, db_path).expect("router construction")
}

async fn assert_renders_identically(
    router: &TaskRouter,
    input: &str,
    task_type: TaskType,
    history: &str,
) {
    let text = router
        .build_prompt(input, &task_type, history)
        .await
        .expect("text prompt");
    let plan = router
        .build_prompt_plan(input, &task_type, history, None, None)
        .await
        .expect("plan");
    assert_eq!(
        plan.render_text(),
        text,
        "PromptPlan::render_text must reproduce build_prompt_with_budget byte-for-byte.\n\
         input={input:?} task_type={task_type:?}",
    );
}

#[tokio::test]
async fn simple_prompt_renders_identically() {
    let tmp = TempDir::new().unwrap();
    let router = router_with_one_file(&tmp);
    assert_renders_identically(
        &router,
        "hello",
        TaskType::Simple,
        "(start of conversation)",
    )
    .await;
}

#[tokio::test]
async fn code_mod_prompt_renders_identically() {
    let tmp = TempDir::new().unwrap();
    let router = router_with_one_file(&tmp);
    assert_renders_identically(
        &router,
        "rename foo to bar",
        TaskType::CodeModification,
        "User: hello\nAssistant: hi\n",
    )
    .await;
}

#[tokio::test]
async fn research_prompt_renders_identically() {
    let tmp = TempDir::new().unwrap();
    let router = router_with_one_file(&tmp);
    assert_renders_identically(
        &router,
        "where is foo defined?",
        TaskType::Research,
        "(start of conversation)",
    )
    .await;
}

/// The cacheable prefix must not depend on the user's request or on the
/// history: that is the invariant the design's golden-prefix test
/// asserts on the raw text form, restated on the type the migration will
/// use.
#[tokio::test]
async fn cacheable_prefix_is_stable_across_turns() {
    let tmp = TempDir::new().unwrap();
    let router = router_with_one_file(&tmp);

    let p1 = router
        .build_prompt_plan(
            "fix the parser",
            &TaskType::CodeModification,
            "(start of conversation)",
            None,
            None,
        )
        .await
        .expect("turn 1 plan");

    let p2 = router
        .build_prompt_plan(
            "and the tests?",
            &TaskType::Testing,
            "User: fix the parser\nAssistant: Looking at it now.\n",
            None,
            None,
        )
        .await
        .expect("turn 2 plan");

    assert!(
        !p1.cacheable_prefix().is_empty(),
        "cacheable prefix must not be empty",
    );
    assert_eq!(
        p1.cacheable_prefix(),
        p2.cacheable_prefix(),
        "cacheable prefix drifted between turns",
    );
}

/// A plan built from a marker-less prompt degrades gracefully: the whole
/// thing becomes a single volatile segment, and rendering still round
/// trips. There is no panic on missing markers.
#[test]
fn from_rendered_handles_missing_marker() {
    let plan = PromptPlan::from_rendered("no marker here at all");
    assert_eq!(plan.system.len(), 1);
    assert!(!plan.system[0].cacheable);
    assert_eq!(plan.render_text(), "no marker here at all");
}

/// Marker present: the split is exactly at the marker, and render
/// round-trips.
#[test]
fn from_rendered_splits_at_marker() {
    let raw = "stable bits\n## Volatile suffix (not cached)\nvolatile bits";
    let plan = PromptPlan::from_rendered(raw);
    assert_eq!(plan.system.len(), 2);
    assert!(plan.system[0].cacheable);
    assert!(!plan.system[1].cacheable);
    assert_eq!(plan.render_text(), raw);
    assert_eq!(plan.cacheable_prefix(), "stable bits\n");
}
