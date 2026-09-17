//! Characterization snapshots for the router prompt (§11.3).
//!
//! # What this is
//!
//! The prompt a session builds for the model is a contract. Every
//! refactor that touches `TaskRouter::build_prompt_with_context` —
//! the D1 `CompletionRequest` migration, a change to the cacheable
//! prefix's shape, a new section — must prove it did not silently
//! change what the model sees, or the change must be reviewed and
//! accepted. These snapshots are that proof.
//!
//! # How it works
//!
//! Each test builds a prompt for a fixed scenario (fixed input,
//! fixed history, fixed working directory), normalizes the
//! scenario-specific bits (the tempdir path), and compares the
//! result byte-for-byte against a file under `tests/snapshots/`.
//!
//! On the *first* run — or when `UPDATE_SNAPSHOTS=1` is set — the
//! file is written instead of compared. Every subsequent run
//! compares. This is the "golden file" pattern; it works with plain
//! `cargo test` and does not require `insta`, `trybuild`, or any
//! other snapshot framework.
//!
//! # Accepting an intentional change
//!
//!     UPDATE_SNAPSHOTS=1 cargo test -p kod-core --test characterization_prompts
//!
//! then commit the modified files under `tests/snapshots/`. A
//! reviewer sees the diff in the PR. If the change was not
//! intentional, the test failed for the right reason.
//!
//! # What the snapshots cover
//!
//! Five scenarios:
//!
//! 1. `simple_fresh_session` — a first turn, no history, no skills,
//!    no memory. The base shape.
//! 2. `simple_with_history` — a second turn, one exchange of
//!    history. The `## Conversation so far` block.
//! 3. `code_modification_with_repo_map` — a code-mod task against a
//!    working directory with two source files. The repo map.
//! 4. `debugging_task_with_history` — a debugging task with
//!    history. The task-type context line.
//! 5. `research_task_no_context` — a research task with no history.
//!
//! Scenarios with memory and skills are deliberately **not**
//! covered here. Memory retrieval is nondeterministic across runs
//! (it depends on the redb store the harness happens to have), and
//! a snapshot that fails one run in ten is worse than no snapshot.
//! The skill-match path is covered by `router.rs`'s own unit tests;
//! the byte shape of the resulting prompt is not what those tests
//! are pinning.

use kod_core::router::{RouterConfig, TaskRouter, TaskType};
use std::path::{Path, PathBuf};
use tempfile::TempDir;

fn snapshots_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("snapshots")
}

/// Replace the actual tempdir path with `<TMP>` in the prompt so the
/// snapshot is stable across machines and runs. The repository map
/// uses *relative* paths already (the walker strips the root), so
/// this normalization is defensive — it exists for the case where a
/// future prompt section happens to include an absolute path, not
/// for a known leak today.
fn normalize(prompt: &str, tmp: &Path) -> String {
    prompt.replace(&tmp.display().to_string(), "<TMP>")
}

/// Write the snapshot on first run or when `UPDATE_SNAPSHOTS=1`,
/// compare otherwise.
fn check_snapshot(name: &str, actual: &str) {
    let dir = snapshots_dir();
    std::fs::create_dir_all(&dir).expect("create snapshots dir");
    let path = dir.join(format!("{name}.txt"));

    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() || !path.exists() {
        std::fs::write(&path, actual).expect("write snapshot");
        eprintln!("[characterization] wrote {}", path.display());
        return;
    }

    let expected = std::fs::read_to_string(&path).expect("read snapshot");
    if expected != actual {
        // Write the actual to a sibling file so the failure message
        // can name both paths and a reviewer can diff them.
        let actual_path = dir.join(format!("{name}.actual.txt"));
        let _ = std::fs::write(&actual_path, actual);
        panic!(
            "\n=== prompt drift in scenario {name:?} ===\n\
             expected: {}\n\
             actual:   {}\n\
             \n\
             If this change is intentional, re-run with UPDATE_SNAPSHOTS=1 \n\
             to accept the new snapshot, and commit the diff under \n\
             tests/snapshots/ alongside the code change. If it is not, \n\
             revert the offending code.",
            path.display(),
            actual_path.display(),
        );
    }
}

/// Router with `enable_memory: false` so `retrieve_context` is a
/// no-op. The base config is `RouterConfig::default()` with only
/// `working_dir` and `enable_memory` overridden — future additions
/// to `RouterConfig` inherit their defaults instead of forcing this
/// helper to be updated.
fn make_router(tmp: &Path) -> TaskRouter {
    let mut cfg = RouterConfig::default();
    cfg.working_dir = tmp.to_path_buf();
    cfg.enable_memory = false;
    let db = tmp.join("test.redb");
    TaskRouter::new(cfg, db).expect("router construction")
}

/// Write two source files into `tmp` so the repository-map section
/// is non-empty. The file names are deterministic, and the walker
/// strips the working-directory prefix, so the rendered map is
/// stable.
fn seed_sources(tmp: &Path) {
    std::fs::write(tmp.join("main.rs"), "fn main() {}\n").expect("write main.rs");
    std::fs::write(
        tmp.join("lib.rs"),
        "pub fn helper(x: u32) -> u32 { x + 1 }\n",
    )
    .expect("write lib.rs");
}

// ---------------------------------------------------------------------------
// Scenario 1: simple, fresh session
// ---------------------------------------------------------------------------

#[tokio::test]
async fn snapshot_simple_fresh_session() {
    let tmp = TempDir::new().unwrap();
    let router = make_router(tmp.path());
    let prompt = router
        .build_prompt_with_context(
            "hello, who are you?",
            &TaskType::Simple,
            "(start of conversation)",
            None,
        )
        .await
        .expect("build prompt");
    let actual = normalize(&prompt, tmp.path());
    check_snapshot("simple_fresh_session", &actual);
}

// ---------------------------------------------------------------------------
// Scenario 2: simple, one exchange of history
// ---------------------------------------------------------------------------

#[tokio::test]
async fn snapshot_simple_with_history() {
    let tmp = TempDir::new().unwrap();
    let router = make_router(tmp.path());
    let history = "User: what is rust?\nAssistant: Rust is a systems \
                   programming language focused on memory safety.\n";
    let prompt = router
        .build_prompt_with_context(
            "is it good for web servers?",
            &TaskType::Simple,
            history,
            None,
        )
        .await
        .expect("build prompt");
    let actual = normalize(&prompt, tmp.path());
    check_snapshot("simple_with_history", &actual);
}

// ---------------------------------------------------------------------------
// Scenario 3: code modification against a working directory with files
// ---------------------------------------------------------------------------

#[tokio::test]
async fn snapshot_code_modification_with_repo_map() {
    let tmp = TempDir::new().unwrap();
    seed_sources(tmp.path());
    let router = make_router(tmp.path());
    let prompt = router
        .build_prompt_with_context(
            "rename the helper function to double",
            &TaskType::CodeModification,
            "(start of conversation)",
            None,
        )
        .await
        .expect("build prompt");
    let actual = normalize(&prompt, tmp.path());
    check_snapshot("code_modification_with_repo_map", &actual);
}

// ---------------------------------------------------------------------------
// Scenario 4: debugging, history present
// ---------------------------------------------------------------------------

#[tokio::test]
async fn snapshot_debugging_task_with_history() {
    let tmp = TempDir::new().unwrap();
    seed_sources(tmp.path());
    let router = make_router(tmp.path());
    let history = "User: run the tests\nAssistant: cargo test failed with \
                   a type mismatch on line 42 of lib.rs\n";
    let prompt = router
        .build_prompt_with_context(
            "debug that error",
            &TaskType::Debugging,
            history,
            None,
        )
        .await
        .expect("build prompt");
    let actual = normalize(&prompt, tmp.path());
    check_snapshot("debugging_task_with_history", &actual);
}

// ---------------------------------------------------------------------------
// Scenario 5: research, no context
// ---------------------------------------------------------------------------

#[tokio::test]
async fn snapshot_research_task_no_context() {
    let tmp = TempDir::new().unwrap();
    let router = make_router(tmp.path());
    let prompt = router
        .build_prompt_with_context(
            "research how tokio's work-stealing scheduler works",
            &TaskType::Research,
            "(start of conversation)",
            None,
        )
        .await
        .expect("build prompt");
    let actual = normalize(&prompt, tmp.path());
    check_snapshot("research_task_no_context", &actual);
}

// ---------------------------------------------------------------------------
// Invariant checks that do not need snapshots
// ---------------------------------------------------------------------------

/// The cacheable prefix is byte-identical between turns — same
/// property `tests/golden_prefix.rs` checks, repeated here so the
/// characterization suite is self-contained (a reader who opens
/// only this file sees the invariant, not a cross-reference).
#[tokio::test]
async fn cacheable_prefix_does_not_drift_between_turns() {
    let tmp = TempDir::new().unwrap();
    seed_sources(tmp.path());
    let router = make_router(tmp.path());
    const MARKER: &str = "## Volatile suffix (not cached)";

    let p1 = router
        .build_prompt_with_context("first", &TaskType::Simple, "(start of conversation)", None)
        .await
        .unwrap();
    let p2 = router
        .build_prompt_with_context(
            "second",
            &TaskType::Simple,
            "User: first\nAssistant: ok\n",
            None,
        )
        .await
        .unwrap();

    let prefix = |s: &str| -> String {
        let idx = s.find(MARKER).unwrap_or(s.len());
        s[..idx].to_string()
    };

    assert_eq!(
        prefix(&p1),
        prefix(&p2),
        "the cacheable prefix drifted between two turns of the same session",
    );
}

/// A second call to `build_prompt_with_context` with the same
/// arguments produces byte-identical output. Guards against any
/// future insertion of a counter, timestamp, or random value into
/// the prompt — the exact class of accidental nondeterminism the
/// doc's prompt-cache section warns about.
#[tokio::test]
async fn prompt_is_deterministic_within_a_router() {
    let tmp = TempDir::new().unwrap();
    seed_sources(tmp.path());
    let router = make_router(tmp.path());

    let a = router
        .build_prompt_with_context("x", &TaskType::Simple, "(start of conversation)", None)
        .await
        .unwrap();
    let b = router
        .build_prompt_with_context("x", &TaskType::Simple, "(start of conversation)", None)
        .await
        .unwrap();
    assert_eq!(a, b, "prompt construction is not deterministic");
}

/// Two routers over the same working directory produce the same
/// prompt. This is what allows a session to restart into a cached
/// prefix.
#[tokio::test]
async fn prompt_is_deterministic_across_routers() {
    let tmp = TempDir::new().unwrap();
    seed_sources(tmp.path());

    let make = |name: &str| {
        let mut cfg = RouterConfig::default();
        cfg.working_dir = tmp.path().to_path_buf();
        cfg.enable_memory = false;
        TaskRouter::new(cfg, tmp.path().join(name)).unwrap()
    };
    let a = make("a.redb");
    let b = make("b.redb");

    let pa = a
        .build_prompt_with_context("x", &TaskType::Simple, "(start of conversation)", None)
        .await
        .unwrap();
    let pb = b
        .build_prompt_with_context("x", &TaskType::Simple, "(start of conversation)", None)
        .await
        .unwrap();
    assert_eq!(pa, pb, "two routers produced different prompts");
}
