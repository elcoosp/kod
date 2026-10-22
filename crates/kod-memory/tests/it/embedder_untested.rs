//! `MemoryManager::set_embedder` and the two `embedder_name` states
//! that `manager_untested` did not cover.
//!
//! A fresh manager has no embedder → `"none"` (covered in
//! `manager_untested::embedder_name_is_none_by_default`). The two
//! remaining branches:
//!
//! - an embedder with `dims() == 0` → `"unusable"`
//! - an embedder with `dims() > 0` → `"configured"`
//!
//! `set_embedder` also clears the vector index and query cache (a new
//! model's vectors are incomparable to the old model's). That side
//! effect is not observable from outside without exposing the index,
//! so the assertions here are on the name transition, which is the
//! branch that `embedder_name` exists to report.
use async_trait::async_trait;
use kod_error::Result;
use kod_memory::embedding::NoEmbedder;
use kod_memory::manager::MemoryManager;
use kod_memory::EmbeddingClient;
use std::sync::Arc;
use tempfile::TempDir;

/// A stub reporting a chosen `dims()`. `embed` is not called by these
/// tests; the impl exists only to satisfy the trait.
struct FakeEmbedder {
    dims: usize,
}

#[async_trait]
impl EmbeddingClient for FakeEmbedder {
    fn name(&self) -> &str {
        "fake"
    }
    fn dims(&self) -> usize {
        self.dims
    }
    async fn embed(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Err(kod_error::KodError::Config(
            "FakeEmbedder does not embed".to_string(),
        ))
    }
}

fn manager_in(dir: &std::path::Path) -> MemoryManager {
    MemoryManager::new(dir.join("m.redb"), 100).expect("manager")
}

/// `NoEmbedder` returns `dims() == 0`, so the manager reports
/// `"unusable"` — the state a caller checks to skip embedding rather
/// than relying on `embed()` returning a sentinel.
#[test]
fn no_embedder_reports_unusable() {
    let dir = TempDir::new().unwrap();
    let mut mgr = manager_in(dir.path());
    mgr.set_embedder(Arc::new(NoEmbedder));
    assert_eq!(mgr.embedder_name(), "unusable");
}

/// A stub with `dims() > 0` reports `"configured"`.
#[test]
fn embedder_with_positive_dims_reports_configured() {
    let dir = TempDir::new().unwrap();
    let mut mgr = manager_in(dir.path());
    mgr.set_embedder(Arc::new(FakeEmbedder { dims: 4 }));
    assert_eq!(mgr.embedder_name(), "configured");
}

/// Replacing an embedder is allowed and the name follows the latest
/// install: configured → unusable.
#[test]
fn replacing_an_embedder_updates_the_name() {
    let dir = TempDir::new().unwrap();
    let mut mgr = manager_in(dir.path());
    mgr.set_embedder(Arc::new(FakeEmbedder { dims: 4 }));
    assert_eq!(mgr.embedder_name(), "configured");
    mgr.set_embedder(Arc::new(NoEmbedder));
    assert_eq!(mgr.embedder_name(), "unusable");
}
