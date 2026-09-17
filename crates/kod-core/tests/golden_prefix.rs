//! Golden-prefix regression tests (D0.6 P7).
//!
//! The cacheable portion of a prompt — the "Identity" preamble plus
//! the repository map — must be byte-identical from one turn to the
//! next within a session. That property is what makes prompt caching
//! work at all: Anthropic's `cache_control` and the implicit
//! prefix-caching on OpenAI-compatible servers both key on a
//! byte-stable prefix. A single character of drift (a timestamp, a
//! counter, an unordered list that reordered) silently defeats the
//! cache and the user pays full input tokens for every turn.
//!
//! # Why an integration test and not a unit test
//!
//! The stable prefix is assembled from three sources the unit tests
//! do not exercise together: the identity preamble (a constant in
//! the router), the repository map (a walk of the working directory
//! with mtime-based invalidation), and the system-segment ordering
//! the router emits. An integration test is the only level at which
//! the full pipeline runs — the same code path a live session
//! exercises.
//!
//! # What the test deliberately does not assert
//!
//! It does not assert the *content* of the identity preamble or the
//! repository map beyond what makes them identifiable. Those change
//! between releases and the test should not break when they do. The
//! property under test is *stability*, not a specific byte string.

use kod_core::router::{RouterConfig, TaskRouter, TaskType};
use tempfile::TempDir;

/// The marker the router emits between the cacheable prefix and the
/// volatile suffix. Everything before this line is supposed to be
/// byte-stable across turns; everything after it changes.
const VOLATILE_MARKER: &str = "## Volatile suffix (not cached)";

/// Build a router rooted at a tempdir with two source files present.
///
/// The files are not compiled — the repository map is a regex walk
/// over text, not a build. They exist so the `## Repository map`
/// section is non-empty, which is what makes the byte-stability
/// assertion meaningful: with no map, the section is omitted and the
/// test would pass trivially.
fn make_router() -> (TempDir, TaskRouter) {
    let tmp = TempDir::new().expect("tempdir");
    std::fs::write(tmp.path().join("main.rs"), "fn main() {}\n").expect("write main.rs");
    std::fs::write(tmp.path().join("lib.rs"), "pub fn hello() {}\n").expect("write lib.rs");

    let db_path = tmp.path().join("test.redb");
    let router = TaskRouter::new(
        RouterConfig {
            working_dir: tmp.path().to_path_buf(),
            enable_memory: false,
            max_skills_per_query: 3,
            context_window: 8192,
            short_term_capacity: 100,
        },
        db_path,
    )
    .expect("router construction");

    (tmp, router)
}

/// Split a rendered prompt at the volatile marker. Panics with a
/// useful message when the marker is absent — a prompt built without
/// it is a bug in the router, and the assertion should say so
/// directly.
fn split_at_marker(prompt: &str) -> (&str, &str) {
    let idx = prompt.find(VOLATILE_MARKER).unwrap_or_else(|| {
        panic!(
            "prompt does not contain the volatile marker {VOLATILE_MARKER:?};\n\
             prompt was:\n{prompt}"
        )
    });
    (&prompt[..idx], &prompt[idx..])
}

/// The main golden test: build a prompt for turn 1, then build a
/// prompt for turn 2 with turn-1 history, and assert that the
/// cacheable prefix is byte-identical between the two.
#[tokio::test]
async fn stable_prefix_is_byte_identical_across_turns() {
    let (_tmp, router) = make_router();
    let task_type = TaskType::Simple;

    let turn1_input = "fix the parser";
    let prompt1 = router
        .build_prompt_with_context(
            turn1_input,
            &task_type,
            "(start of conversation)",
            None,
        )
        .await
        .expect("build turn 1 prompt");

    let history =
        "User: fix the parser\nAssistant: Done, the parser is fixed.\n";
    let turn2_input = "and the tests?";
    let prompt2 = router
        .build_prompt_with_context(turn2_input, &task_type, history, None)
        .await
        .expect("build turn 2 prompt");

    let (prefix1, suffix1) = split_at_marker(&prompt1);
    let (prefix2, suffix2) = split_at_marker(&prompt2);

    assert_eq!(
        prefix1, prefix2,
        "cacheable prefix drifted between turn 1 and turn 2.\n\
         turn 1 prefix ({} bytes):\n{}\n\
         turn 2 prefix ({} bytes):\n{}",
        prefix1.len(),
        prefix1,
        prefix2.len(),
        prefix2,
    );

    // The volatile suffix must reflect the current turn — otherwise
    // the "stable prefix" assertion above is vacuous (the whole
    // prompt would be identical, meaning the user's new input never
    // reached it).
    assert!(
        suffix2.contains(turn2_input),
        "turn 2's suffix must contain turn 2's input {turn2_input:?};\n\
         suffix was:\n{suffix2}",
    );
    assert!(
        suffix1.contains(turn1_input),
        "turn 1's suffix must contain turn 1's input {turn1_input:?}",
    );
    // And the history that turn 2 was built with must be present in
    // turn 2's suffix.
    assert!(
        suffix2.contains("Done, the parser is fixed"),
        "turn 2's suffix must contain the history it was built with",
    );
}

/// The cacheable prefix must contain the identity preamble and the
/// repository map. Without those, byte-stability would be trivially
/// satisfied by an empty prefix — the exact failure mode the router
/// is supposed to avoid.
#[tokio::test]
async fn stable_prefix_contains_identity_and_repo_map() {
    let (_tmp, router) = make_router();
    let task_type = TaskType::Simple;

    let prompt = router
        .build_prompt_with_context(
            "hello",
            &task_type,
            "(start of conversation)",
            None,
        )
        .await
        .expect("build prompt");

    let (prefix, _suffix) = split_at_marker(&prompt);

    assert!(
        prefix.contains("## Identity"),
        "identity section missing from the cacheable prefix",
    );
    assert!(
        prefix.contains("You are kod"),
        "identity preamble text missing from the cacheable prefix",
    );
    assert!(
        prefix.contains("## Stable prefix (cacheable)"),
        "the cacheable marker must be inside the stable prefix itself",
    );
    assert!(
        prefix.contains("## Repository map"),
        "repository map section missing; the test fixture wrote main.rs and lib.rs, so the map should be non-empty",
    );
    // The two source files should be listed in the map. Their names
    // are the only content the test can assert without coupling to
    // the exact rendering format.
    assert!(
        prefix.contains("main.rs"),
        "repository map must list main.rs",
    );
    assert!(
        prefix.contains("lib.rs"),
        "repository map must list lib.rs",
    );
}

/// A turn whose working tree has not changed must produce the same
/// repository map section as a previous turn — the D0.5 fingerprint
/// check must not spuriously invalidate on unrelated activity.
#[tokio::test]
async fn repo_map_section_is_stable_within_a_session() {
    let (_tmp, router) = make_router();
    let task_type = TaskType::Simple;

    let prompt_a = router
        .build_prompt_with_context("first", &task_type, "(start of conversation)", None)
        .await
        .expect("build prompt a");
    let prompt_b = router
        .build_prompt_with_context(
            "second",
            &task_type,
            "User: first\nAssistant: ok\n",
            None,
        )
        .await
        .expect("build prompt b");

    let (prefix_a, _) = split_at_marker(&prompt_a);
    let (prefix_b, _) = split_at_marker(&prompt_b);

    // Extract just the repository-map section from each and compare.
    // The map is between `## Repository map\n\n` and the next `\n\n## `.
    fn map_section(prefix: &str) -> &str {
        let start = prefix
            .find("## Repository map")
            .expect("repo map section present");
        let rest = &prefix[start..];
        let end = rest[2..].find("\n\n## ").map(|i| i + 2).unwrap_or(rest.len());
        &rest[..end]
    }

    assert_eq!(
        map_section(prefix_a),
        map_section(prefix_b),
        "repository map section changed between turns with an unchanged tree",
    );
}

/// Two routers constructed over the same working directory must
/// produce the same repository map — the D0.5 cache is per-router,
/// and cross-router determinism is the property that lets a session
/// restart into the same cached prefix.
#[tokio::test]
async fn repo_map_is_deterministic_across_routers() {
    let tmp = TempDir::new().expect("tempdir");
    std::fs::write(tmp.path().join("main.rs"), "fn main() {}\n").unwrap();
    std::fs::write(tmp.path().join("lib.rs"), "pub fn hello() {}\n").unwrap();

    let make = |name: &str| {
        let db = tmp.path().join(name);
        TaskRouter::new(
            RouterConfig {
                working_dir: tmp.path().to_path_buf(),
                enable_memory: false,
                max_skills_per_query: 3,
                context_window: 8192,
                short_term_capacity: 100,
            },
            db,
        )
        .unwrap()
    };

    let a = make("a.redb");
    let b = make("b.redb");

    let task_type = TaskType::Simple;
    let pa = a
        .build_prompt_with_context("x", &task_type, "(start of conversation)", None)
        .await
        .unwrap();
    let pb = b
        .build_prompt_with_context("x", &task_type, "(start of conversation)", None)
        .await
        .unwrap();

    let (prefix_a, _) = split_at_marker(&pa);
    let (prefix_b, _) = split_at_marker(&pb);

    assert_eq!(
        prefix_a, prefix_b,
        "two routers over the same tree produced different cacheable prefixes",
    );
}
