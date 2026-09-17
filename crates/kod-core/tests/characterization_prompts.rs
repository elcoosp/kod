//! Characterization snapshots for the router prompt (§11.3).
//!
//! # What this is
//!
//! The prompt a session builds for the model is a contract. Every
//! refactor that touches `TaskRouter::build_prompt_with_context`
//! must prove it did not silently change what the model sees.
//! These snapshots are that proof.
//!
//! # How it works
//!
//! Each test builds a prompt for a fixed scenario (fixed input,
//! fixed history, fixed working directory), normalizes the
//! tempdir-specific bits, and compares the result byte-for-byte
//! against a file under `tests/snapshots/`. On the first run — or
//! when `UPDATE_SNAPSHOTS=1` is set — the file is written instead
//! of compared. Every subsequent run compares. This is the
//! golden-file pattern; it works with plain `cargo test` and does
//! not require `insta` or `trybuild`.
//!
//! # Accepting an intentional change
//!
//!     UPDATE_SNAPSHOTS=1 cargo test -p kod-core --test characterization_prompts
//!
//! then commit the modified files under `tests/snapshots/`. A
//! reviewer sees the diff in the PR.
//!
//! # Deliberately not covered
//!
//! Memory and skill scenarios are excluded. Retrieval against the
//! redb store depends on the store's prior contents (nondeterministic
//! across machines), and a skill's instructions embed its absolute
//! path (per-machine). A snapshot that flakes is worse than no
//! snapshot; those paths have their own unit tests.

use kod_core::router::{RouterConfig, TaskRouter, TaskType};
use std::path::{Path, PathBuf};
use tempfile::TempDir;

fn snapshots_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("snapshots")
}

/// Replace the actual tempdir path with `<TMP>` so the snapshot is
/// stable across machines. The repository map already uses relative
/// paths; this is defensive against a future section that happens
/// to include an absolute path.
fn normalize(prompt: &str, tmp: &Path) -> String {
    prompt.replace(&tmp.display().to_string(), "<TMP>")
}

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
        let actual_path = dir.join(format!("{name}.actual.txt"));
        let _ = std::fs::write(&actual_path, actual);
        panic!(
            "\n=== prompt drift in scenario {name:?} ===\n\
             expected: {}\n\
             actual:   {}\n\n\
             If this is intentional, re-run with UPDATE_SNAPSHOTS=1 and commit\n\
             the diff. If it is not, revert the offending code.",
            path.display(),
            actual_path.display(),
        );
    }
}

fn make_router(tmp: &Path) -> TaskRouter {
    let mut cfg = RouterConfig::default();
    cfg.working_dir = tmp.to_path_buf();
    cfg.enable_memory = false;
    let db = tmp.join("test.redb");
    TaskRouter::new(cfg, db).expect("router construction")
}

fn seed_sources(tmp: &Path) {
    std::fs::write(tmp.join("main.rs"), "fn main() {}\n").expect("write main.rs");
    std::fs::write(
        tmp.join("lib.rs"),
        "pub fn helper(x: u32) -> u32 { x + 1 }\n",
    )
    .expect("write lib.rs");
}

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
    check_snapshot("simple_fresh_session", &normalize(&prompt, tmp.path()));
}

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
    check_snapshot("simple_with_history", &normalize(&prompt, tmp.path()));
}

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
    check_snapshot(
        "code_modification_with_repo_map",
        &normalize(&prompt, tmp.path()),
    );
}

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
    check_snapshot("debugging_task_with_history", &normalize(&prompt, tmp.path()));
}

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
    check_snapshot("research_task_no_context", &normalize(&prompt, tmp.path()));
}

/// The cacheable prefix is byte-identical between turns — the same
/// invariant `tests/golden_prefix.rs` checks, repeated here so the
/// characterization suite is self-contained.
#[tokio::test]
async fn cacheable_prefix_does_not_drift_between_turns() {
    let tmp = TempDir::new().unwrap();
    seed_sources(tmp.path());
    let router = make_router(tmp.path());
    const MARKER: &str = "## Volatile suffix (not cached)";

    let p1 = router
        .build_prompt_with_context(
            "first",
            &TaskType::Simple,
            "(start of conversation)",
            None,
        )
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

    assert_eq!(prefix(&p1), prefix(&p2), "cacheable prefix drifted");
}

/// Same inputs, same output — no hidden counter or timestamp.
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

/// Two routers over the same tree produce the same prompt.
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
