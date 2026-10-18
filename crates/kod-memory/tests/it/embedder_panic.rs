//! A panicking embedder during retrieval must not kill the caller's
//! turn (session 18, fault-injection arc).
//!
//! The background embed path is already isolated:
//! `flush_returns_after_panicking_embed_task` (manager.rs test module)
//! proves a panic in a spawned embed task does not wedge the manager.
//! The *foreground* path was not. `retrieve_context` ->
//! `retrieve_long_term_hybrid` -> `rebuild_index` -> `embed` runs on
//! every prompt when an embedder is installed and the vector index is
//! cold. That embed call had no panic isolation: a panic in a
//! third-party embedding client (manager.rs:699 names "a reqwest
//! panic" as the known cause) unwinds through the retrieval path and
//! out of the caller's turn — the same class as the tool-registry
//! panic fixed in kod-tools (commit 1dbf8cc).
//!
//! Embedding is optional. The `Err` arm already falls through to
//! keyword+recency scoring; a panic must do the same, not unwind.
//!
//! Note the trap this test fell into on first draft: an *empty*
//! long-term store makes `retrieve_long_term_hybrid` return early
//! without ever reaching the embed call, so the test passed
//! vacuously. Storing one entry first gets past the early return and
//! into the embed path the test is meant to exercise.

use async_trait::async_trait;
use kod_error::Result;
use kod_memory::{EmbeddingClient, MemoryManager};
use kod_types::memory::MemoryType;

/// An embedder that panics on every call. The fixture, not a
/// workaround.
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
        panic!("foreground retrieval: deliberate embed panic for the isolation test");
    }
}

#[tokio::test]
async fn panicking_embedder_does_not_kill_the_retrieval_path() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut mgr = MemoryManager::new(dir.path().join("retrieval.redb"), 100)
        .expect("manager construction");
    mgr.set_embedder(std::sync::Arc::new(PanicEmbedder));

    // Store one entry. This puts a row in the long-term store so
    // `retrieve_long_term_hybrid` gets past its `all.is_empty()`
    // early-return. The background embed this store spawns is already
    // guarded by the flush-drop test; it panics asynchronously and is
    // caught by the existing isolation. What this test pins is the
    // *foreground* embed `rebuild_index` fires on retrieval.
    let _ = mgr
        .store_with_metadata(MemoryType::LongTerm, "one stored entry", Default::default())
        .await;

    // If the foreground embed call is not isolated, this `.await`
    // unwinds and the test fails by panic — not by assertion.
    let ctx = mgr
        .retrieve_context("any query with semantic availability")
        .await;

    assert!(
        ctx.is_ok(),
        "a panicking embedder must degrade to keyword+recency, not unwind \
         the retrieval path; got {ctx:?}",
    );
    let _ = ctx.unwrap();
}
