//! Golden prefix tests (D0.6).
//!
//! The prompt-cache principle (roadmap principle n°2) requires that the
//! cacheable prefix of a prompt be byte-identical between consecutive
//! turns in the same session. The provider and the model see a stable
//! prefix and can reuse KV-cache state (OpenAI automatic prompt caching,
//! Anthropic `cache_control`); if the prefix drifts by even one byte,
//! every turn pays full-price token cost and, on slow local models,
//! every turn re-reads the repo map.
//!
//! These tests run against the current `build_prompt` — the monolithic
//! string returned by the router. When D1 replaces the string with a
//! structured `PromptPlan` (AD-16), the same two assertions are
//! rewritten to compare the concatenation of cacheable segments; the
//! invariant tested here is what stays constant across the refactor.
//!
//! These are the first integration tests in the workspace (a `tests/`
//! directory at last — the missing target that CI job `live-model`
//! referenced in B3). They are also the safety net for the A3 refactor
//! of `KodEngine` (three months from now): any drift in the stable
//! prefix surfaces here, before it propagates to every prompt.

use kod_core::router::{RouterConfig, TaskRouter, TaskType};
use tempfile::TempDir;

/// Marker that separates the cacheable prefix from the volatile suffix
/// in the router's monolithic prompt string.
const VOLATILE_MARKER: &str = "## Volatile suffix (not cached)";

/// Split a prompt into (cacheable_prefix, volatile_suffix). The marker
/// itself is excluded from the prefix — everything before the marker
/// is what a provider hashes for caching.
fn split_prompt(prompt: &str) -> (&str, &str) {
    match prompt.find(VOLATILE_MARKER) {
        Some(idx) => (&prompt[..idx], &prompt[idx..]),
        None => (prompt, ""),
    }
}

/// Build a router rooted at `wd` with memory disabled. Memory retrieval
/// would introduce nondeterminism (short-term recency depends on the
/// wall clock); the prefix invariant is about identity + repo map, and
/// those are what the test isolates.
fn router_for(wd: &std::path::Path) -> TaskRouter {
    let db_path = wd.join("test.redb");
    TaskRouter::new(
        RouterConfig {
            working_dir: wd.to_path_buf(),
            enable_memory: false,
            max_skills_per_query: 3,
            context_window: 8192,
            short_term_capacity: 100,
        },
        db_path,
    )
    .expect("TaskRouter::new")
}

/// Seed a minimal workspace so the repo map is non-empty. The map is
/// part of the cacheable prefix; an empty map produces an empty prefix
/// section, which weakens the test's signal.
fn seed_repo(wd: &std::path::Path) {
    std::fs::write(
        wd.join("main.rs"),
        "pub fn main() {}\nstruct Engine;\n",
    )
    .unwrap();
    std::fs::write(
        wd.join("lib.rs"),
        "pub fn helper() {}\npub struct Config;\n",
    )
    .unwrap();
}

#[tokio::test]
async fn stable_prefix_is_byte_identical_across_turns() {
    let temp_dir = TempDir::new().unwrap();
    let wd = temp_dir.path().to_path_buf();
    seed_repo(&wd);
    let router = router_for(&wd);

    // Turn 1: fresh session.
    let prompt_1 = router
        .build_prompt_with_context(
            "fix the parser",
            &TaskType::CodeModification,
            "(start of conversation)",
            None,
        )
        .await
        .expect("turn 1 prompt");

    // Turn 2: same session, history contains one exchange.
    let history_2 = "User: fix the parser\nAssistant: which file?\n";
    let prompt_2 = router
        .build_prompt_with_context(
            "and the tests?",
            &TaskType::Testing,
            history_2,
            None,
        )
        .await
        .expect("turn 2 prompt");

    let (prefix_1, volatile_1) = split_prompt(&prompt_1);
    let (prefix_2, volatile_2) = split_prompt(&prompt_2);

    // The invariant: the stable prefix is identical byte-for-byte.
    assert_eq!(
        prefix_1.len(),
        prefix_2.len(),
        "cacheable prefix length drifted: {} vs {}",
        prefix_1.len(),
        prefix_2.len()
    );
    assert_eq!(
        prefix_1, prefix_2,
        "cacheable prefix is not byte-identical across turns"
    );

    // Sanity: the volatile parts actually differ (otherwise the test
    // would pass on two identical prompts and prove nothing).
    assert_ne!(
        volatile_1, volatile_2,
        "volatile suffixes are identical — test setup is degenerate"
    );

    // Sanity: both prefixes are non-trivial (identity + repo map).
    assert!(
        prefix_1.contains("You are kod"),
        "prefix is missing the identity preamble: {prefix_1}"
    );
    assert!(
        prefix_1.contains("main.rs"),
        "prefix is missing the repo map: {prefix_1}"
    );
}

#[tokio::test]
async fn stable_prefix_does_not_contain_user_input() {
    // The user's prompt goes in the volatile suffix. If it appeared in
    // the prefix, prompt caching would be defeated on every turn (the
    // cache key changes with each new prompt).
    let temp_dir = TempDir::new().unwrap();
    let wd = temp_dir.path().to_path_buf();
    seed_repo(&wd);
    let router = router_for(&wd);

    let marker = "ZZZ_USER_PROMPT_MARKER_ZZZ";
    let prompt = router
        .build_prompt_with_context(
            marker,
            &TaskType::Simple,
            "(start of conversation)",
            None,
        )
        .await
        .expect("prompt");

    let (prefix, volatile) = split_prompt(&prompt);
    assert!(
        !prefix.contains(marker),
        "user input leaked into cacheable prefix: {prefix}"
    );
    assert!(
        volatile.contains(marker),
        "user input missing from volatile suffix: {volatile}"
    );
}

#[tokio::test]
async fn build_prompt_delegator_matches_with_context_none() {
    // `build_prompt` (legacy) must produce the same output as
    // `build_prompt_with_context(..., None)`. When D1's refactor
    // replaces the string with a PromptPlan, this test is the first
    // thing to break if the delegator drifts.
    let temp_dir = TempDir::new().unwrap();
    let wd = temp_dir.path().to_path_buf();
    seed_repo(&wd);
    let router = router_for(&wd);

    let input = "explain the architecture";
    let history = "(start of conversation)";

    let a = router
        .build_prompt(input, &TaskType::Simple, history)
        .await
        .expect("build_prompt");
    let b = router
        .build_prompt_with_context(input, &TaskType::Simple, history, None)
        .await
        .expect("build_prompt_with_context(None)");

    assert_eq!(a, b, "build_prompt and build_prompt_with_context diverge");
}

#[tokio::test]
async fn prefix_survives_history_growth() {
    // Turn N with a growing history must keep the same prefix as turn 1.
    // This is the "cache miss on every turn" failure mode the roadmap
    // calls out for OpenCode; a passing test here means Kod does not
    // inherit it.
    let temp_dir = TempDir::new().unwrap();
    let wd = temp_dir.path().to_path_buf();
    seed_repo(&wd);
    let router = router_for(&wd);

    let prompt_1 = router
        .build_prompt_with_context("first", &TaskType::Simple, "(start of conversation)", None)
        .await
        .unwrap();
    let (prefix_1, _) = split_prompt(&prompt_1);

    // Simulate 20 turns of history.
    let mut history = String::new();
    for i in 0..20 {
        history.push_str(&format!("User: message {i}\nAssistant: reply {i}\n"));
    }
    let prompt_n = router
        .build_prompt_with_context("twentieth", &TaskType::Simple, &history, None)
        .await
        .unwrap();
    let (prefix_n, _) = split_prompt(&prompt_n);

    assert_eq!(
        prefix_1, prefix_n,
        "cacheable prefix drifted after 20 turns of history"
    );
}
