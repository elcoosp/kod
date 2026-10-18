//! A panicking embedder during consolidation must not kill the
//! maintenance pass (session 18, fault-injection arc, completion).
//!
//! Companion to `embedder_panic.rs` (the retrieval path). Retrieval
//! runs on every prompt; consolidation runs from the router
//! (`router.rs:527`) as a maintenance pass. Both call the embedder on
//! the caller's task — no spawn boundary — so both had the same
//! unwind risk. `embed_safe` was added for retrieval; this test pins
//! the other foreground call site (`fuse_duplicates`) once it too is
//! wrapped.
//!
//! Two traps this test had to clear:
//!
//! 1. `fuse_duplicates` skips any project group with fewer than two
//!    entries. Two *identical* strings collapse to one at the store
//!    layer, so the group has one entry and the embed call never
//!    fires — the test passes vacuously. The two contents must
//!    differ.
//! 2. The fuse sort is by `last_retrieved_at_ms` with a wall-clock
//!    fallback at 1-second resolution. Explicit timestamps remove the
//!    wall-clock dependency.
//!
//! With both cleared, `consolidate()` reaches
//! `fuse_duplicates`'s embed call, which is the site this test pins.

use async_trait::async_trait;
use kod_error::Result;
use kod_memory::{EmbeddingClient, MemoryManager};
use kod_types::memory::{MemoryMetadata, MemoryType};

struct PanicEmbedder;

#[async_trait]
impl EmbeddingClient for PanicEmbedder {
    fn name(&self) -> &str {
        "panic"
    }
    fn dims(&self) -> usize {
        2
    }
    async fn embed(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>> {
        panic!("consolidation: deliberate embed panic for the isolation test");
    }
}

#[tokio::test]
async fn panicking_embedder_does_not_kill_consolidation() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut mgr =
        MemoryManager::new(dir.path().join("consolidate.redb"), 100).expect("manager");
    mgr.set_embedder(std::sync::Arc::new(PanicEmbedder));

    // Two DIFFERENT entries in the same project. Different content
    // defeats the store's dedup, so `fuse_duplicates` sees a group of
    // two and reaches its embed call. Explicit timestamps remove the
    // fuse sort's wall-clock dependency.
    let meta_a = MemoryMetadata {
        project_key: Some("proj".to_string()),
        last_retrieved_at_ms: Some(100),
        ..Default::default()
    };
    let meta_b = MemoryMetadata {
        project_key: Some("proj".to_string()),
        last_retrieved_at_ms: Some(200),
        ..Default::default()
    };
    let _ = mgr
        .store_with_metadata(
            MemoryType::LongTerm,
            "the parser retries on transient failures",
            meta_a,
        )
        .await;
    let _ = mgr
        .store_with_metadata(
            MemoryType::LongTerm,
            "the parser backoff doubles on each retry",
            meta_b,
        )
        .await;

    // The background embeds spawned by the stores panic asynchronously
    // and are isolated (existing flush test). `consolidate` awaits
    // them, then calls `fuse_duplicates`, whose foreground embed is
    // the site this test pins.
    let report = mgr.consolidate().await;
    assert!(
        report.is_ok(),
        "a panicking embedder must skip the affected group, not unwind \
         the consolidation pass; got {report:?}",
    );
}
