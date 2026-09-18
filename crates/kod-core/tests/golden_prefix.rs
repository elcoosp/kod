//! Golden-prefix test (design §3, D0.6).
//!
//! Locks the byte-stability of the router prompt's cacheable region
//! across turns in the same session. Providers that cache on a
//! byte-stable prefix (OpenAI-compatible automatic prefix caching,
//! Anthropic explicit `cache_control`) only pay off if the prefix
//! really is stable; a router that emits a byte that shifts with each
//! request silently disables the cache and burns input tokens on every
//! turn.
//!
//! The design puts this test *before* the D1 engine migration so the
//! refactor cannot quietly regress the invariant. After `build_prompt`
//! becomes `build_prompt_plan` (AD-16), the same assertion must be
//! re-expressed on the concatenation of `PromptPlan::system` segments
//! flagged `cacheable` — but the invariant this file pins (the
//! cacheable region is a function of the session, not of the current
//! request or of past turns) is what matters, not the concrete marker.
//!
//! ## What "cacheable region" means here
//!
//! `TaskRouter::build_prompt_with_budget` emits the prompt in two
//! textually delimited blocks:
//!
//! ```text
//! ## Identity
//! ...
//! ## Stable prefix (cacheable)
//! ...
//! ## Repository map
//! ...
//! ## Volatile suffix (not cached)
//! ...
//! ```
//!
//! Everything up to and including the byte *before* the volatile
//! marker is the cacheable region. The test compares exactly that
//! substring.

use kod_core::router::{RouterConfig, TaskRouter, TaskType};
use tempfile::TempDir;

/// The marker that opens the volatile block. Everything strictly
/// before its first occurrence is the cacheable region.
const VOLATILE_MARKER: &str = "## Volatile suffix";

/// Split a rendered prompt into `(cacheable, rest)`. Panics with a
/// clear message when the marker is missing — the test is asserting
/// an invariant of the router, so a missing marker is a router
/// regression, not a test setup error.
fn split_cacheable(prompt: &str) -> (&str, &str) {
    let idx = prompt
        .find(VOLATILE_MARKER)
        .unwrap_or_else(|| panic!(
            "router prompt is missing the volatile marker {VOLATILE_MARKER:?} \
             — the cacheable prefix invariant no longer applies"
        ));
    (&prompt[..idx], &prompt[idx..])
}

/// Build a fresh router rooted in a tempdir with a single `.rs` file so
/// the repo-map block is non-empty (an empty map would render nothing
/// and the test would trivially pass).
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

#[tokio::test]
async fn stable_prefix_is_byte_identical_across_turns() {
    let tmp = TempDir::new().unwrap();
    let router = router_with_one_file(&tmp);

    // Turn 1: first request of the session, empty history.
    let p1 = router
        .build_prompt(
            "fix the parser",
            &TaskType::CodeModification,
            "(start of conversation)",
        )
        .await
        .expect("turn 1 prompt");

    // Turn 2: different request, history has grown by one exchange.
    let p2 = router
        .build_prompt(
            "and the tests?",
            &TaskType::Testing,
            "User: fix the parser\nAssistant: Looking at it now.\n",
        )
        .await
        .expect("turn 2 prompt");

    let (cacheable_1, _) = split_cacheable(&p1);
    let (cacheable_2, _) = split_cacheable(&p2);

    assert!(
        !cacheable_1.is_empty(),
        "cacheable region must not be empty — the router would emit no prefix",
    );
    assert_eq!(
        cacheable_1,
        cacheable_2,
        "cacheable prefix drifted between turns.\n\
         Turn 1 prefix ({} bytes):\n{}\n---\n\
         Turn 2 prefix ({} bytes):\n{}\n",
        cacheable_1.len(),
        cacheable_1,
        cacheable_2.len(),
        cacheable_2,
    );

    // The volatile regions must *not* be identical — a router that
    // ignores its inputs would also produce a stable prefix, and this
    // asserts that the test is exercising real behaviour.
    let (_, volatile_1) = split_cacheable(&p1);
    let (_, volatile_2) = split_cacheable(&p2);
    assert_ne!(
        volatile_1, volatile_2,
        "volatile regions should differ across turns: the router is not \
         seeing the request/history arguments at all",
    );
}

#[tokio::test]
async fn stable_prefix_survives_a_history_only_change() {
    // Same request, only the history differs. The prefix must still be
    // stable; the volatile region must still differ.
    let tmp = TempDir::new().unwrap();
    let router = router_with_one_file(&tmp);

    let p1 = router
        .build_prompt("hello", &TaskType::Simple, "(start of conversation)")
        .await
        .expect("turn 1");

    let p2 = router
        .build_prompt(
            "hello",
            &TaskType::Simple,
            "User: hi\nAssistant: hello\n",
        )
        .await
        .expect("turn 2");

    let (c1, v1) = split_cacheable(&p1);
    let (c2, v2) = split_cacheable(&p2);

    assert_eq!(
        c1, c2,
        "cacheable prefix must not depend on history",
    );
    assert_ne!(
        v1, v2,
        "volatile region should have absorbed the history change",
    );
}
