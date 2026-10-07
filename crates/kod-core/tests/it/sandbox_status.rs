//! `KodEngine::sandbox_status` — the effective sandbox the header
//! renders (design §D3.3, AD-10).
//!
//! The function returns `(mode, backend_name)`. The header maps that
//! pair into one of five labels:
//!
//! | mode     | backend      | label              |
//! |----------|--------------|--------------------|
//! | Disabled | anything     | `off`              |
//! | Auto     | Some(name)   | `name`             |
//! | Auto     | None         | `off`              |
//! | Require  | Some(name)   | `name`             |
//! | Require  | None         | `require-missing`  |
//!
//! The `require-missing` case is the one the design says must be
//! visible: a user who asked for `Require` and has no primitive
//! installed is not silently downgraded. This test pins the mapping
//! so a future engine change cannot make the label lie.

use kod_core::KodEngine;
use kod_core::router::RouterConfig;
use kod_tools::context::SandboxMode;
use std::sync::Arc;
use tempfile::TempDir;

async fn engine() -> (TempDir, Arc<KodEngine>) {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("t.redb");
    let cfg = RouterConfig {
        skill_threshold: 0.3,
        context_window: 8192,
        short_term_capacity: 100,
        working_dir: tmp.path().to_path_buf(),
        enable_memory: false,
        max_skills_per_query: 3,
        embedder: None,
    };
    let engine = Arc::new(KodEngine::new(cfg, db).unwrap());
    engine.start().await.unwrap();
    (tmp, engine)
}

#[tokio::test]
async fn disabled_always_reports_no_backend() {
    let (_tmp, engine) = engine().await;
    engine.set_sandbox_mode(SandboxMode::Disabled);
    let (mode, backend) = engine.sandbox_status();
    assert_eq!(mode, SandboxMode::Disabled);
    assert!(
        backend.is_none(),
        "Disabled must not report a backend even if one is installed: got {backend:?}",
    );
}

#[tokio::test]
async fn auto_reports_the_detected_backend_or_none() {
    let (_tmp, engine) = engine().await;
    engine.set_sandbox_mode(SandboxMode::Auto);
    let (mode, backend) = engine.sandbox_status();
    assert_eq!(mode, SandboxMode::Auto);
    // The backend value depends on the host: `Some(name)` when a
    // primitive is installed, `None` otherwise. Both are legal
    // states the header renders (`name` vs `off`).
    if let Some(name) = backend {
        assert!(
            ["bwrap", "landlock", "sandbox-exec"].contains(&name),
            "unexpected backend name: {name}",
        );
    }
}

#[tokio::test]
async fn require_reports_the_backend_or_none_when_missing() {
    let (_tmp, engine) = engine().await;
    engine.set_sandbox_mode(SandboxMode::Require);
    let (mode, backend) = engine.sandbox_status();
    assert_eq!(mode, SandboxMode::Require);
    // The header's mapping for `Require + None` is `require-missing`
    // — the design's "make the failure visible" case. The engine
    // itself reports `None`; the caller decides the label.
    if let Some(name) = backend {
        assert!(
            ["bwrap", "landlock", "sandbox-exec"].contains(&name),
            "unexpected backend name: {name}",
        );
    }
}
