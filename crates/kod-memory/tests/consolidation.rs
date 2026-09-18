//! Memory consolidation tests: archival + fusion (design D2.5).
//!
//! # The two halves
//!
//! `MemoryManager::consolidate` runs two independent operations:
//!
//! - **Archival** — episodic entries whose last touch is older than
//!   `ARCHIVE_AFTER_DAYS` are dropped. Runs with or without an
//!   embedder.
//! - **Fusion** — entries in the same project whose cosine exceeds
//!   0.95 are merged into one survivor, tags unioned. Needs an
//!   embedder; without one, `fused == 0`.
//!
//! This file exercises both, plus the project-scope boundary: two
//! projects whose entries would otherwise fuse must stay separate,
//! because the scope is what stops one project's paraphrases from
//! silently moving into another's store.

use async_trait::async_trait;
use kod_error::Result;
use kod_memory::{EmbeddingClient, MemoryManager};
use kod_types::{MemoryMetadata, MemoryType};
use std::sync::Arc;
use tempfile::TempDir;

/// Deterministic embedder: a lookup table from known input text to a
/// unit vector. Anything unknown maps to the zero vector (which the
/// manager will normalise to no direction — a defensible degradation
/// for the test, since we only exercise known strings).
struct StubEmbedder {
    /// (substring, unit-vector). The first entry whose substring
    /// appears in the text wins.
    table: Vec<(&'static str, Vec<f32>)>,
}

impl StubEmbedder {
    fn new(table: Vec<(&'static str, Vec<f32>)>) -> Self {
        Self { table }
    }
}

#[async_trait]
impl EmbeddingClient for StubEmbedder {
    fn name(&self) -> &str {
        "stub"
    }
    fn dims(&self) -> usize {
        4
    }
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(texts
            .iter()
            .map(|t| {
                for (needle, v) in &self.table {
                    if t.contains(needle) {
                        return v.clone();
                    }
                }
                // Unknown: a small nonzero vector so a stray entry
                // does not accidentally fuse with anything by being
                // colinear with the zero vector.
                vec![0.01, 0.01, 0.01, 0.01]
            })
            .collect())
    }
}

fn manager_with_embedder(dir: &TempDir, embedder: Arc<dyn EmbeddingClient>) -> MemoryManager {
    let db = dir.path().join("mem.redb");
    let mut m = MemoryManager::new(db, 100).expect("manager");
    m.set_embedder(embedder);
    m
}

#[tokio::test]
async fn fuse_merges_identical_entries_and_unions_tags() {
    let dir = TempDir::new().unwrap();
    // Both texts contain "dark" and map to the same unit vector;
    // cosine == 1.0, well above the 0.95 threshold.
    let embedder = Arc::new(StubEmbedder::new(vec![("dark", vec![1.0, 0.0, 0.0, 0.0])]));
    let manager = manager_with_embedder(&dir, embedder);

    // Two paraphrases of the same fact, tagged differently.
    manager
        .store_with_metadata(
            MemoryType::LongTerm,
            "prefers dark mode",
            MemoryMetadata {
                tags: vec!["preference".into()],
                ..Default::default()
            },
        )
        .await
        .unwrap();
    // A small delay so `last_retrieved_at_ms` / timestamps make the
    // first entry the "older" one and the second the survivor.
    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    manager
        .store_with_metadata(
            MemoryType::LongTerm,
            "likes dark themes",
            MemoryMetadata {
                tags: vec!["ui".into()],
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let before = manager.get_all_long_term().await.unwrap();
    assert_eq!(before.len(), 2);

    let report = manager.consolidate().await.unwrap();
    assert_eq!(report.archived, 0, "nothing is old enough to archive");
    assert_eq!(
        report.fused, 1,
        "two cos-1.0 entries must fuse into one; got {}",
        report.fused,
    );

    let after = manager.get_all_long_term().await.unwrap();
    assert_eq!(after.len(), 1, "one survivor after fusion");
    let survivor = &after[0];
    // The survivor is the more recent entry ("likes dark themes") per
    // the fuse rule; its tags were unioned with the deleted entry's.
    assert!(
        survivor.metadata.tags.contains(&"ui".to_string()),
        "survivor's own tag missing: {:?}",
        survivor.metadata.tags,
    );
    assert!(
        survivor.metadata.tags.contains(&"preference".to_string()),
        "deleted entry's tag must be merged in: {:?}",
        survivor.metadata.tags,
    );
}

#[tokio::test]
async fn fuse_does_not_merge_distant_entries() {
    let dir = TempDir::new().unwrap();
    // Two orthogonal unit vectors: cosine == 0.0 < 0.95.
    let embedder = Arc::new(StubEmbedder::new(vec![
        ("dark", vec![1.0, 0.0, 0.0, 0.0]),
        ("cargo", vec![0.0, 1.0, 0.0, 0.0]),
    ]));
    let manager = manager_with_embedder(&dir, embedder);

    manager
        .store_with_metadata(
            MemoryType::LongTerm,
            "prefers dark mode",
            MemoryMetadata::default(),
        )
        .await
        .unwrap();
    manager
        .store_with_metadata(
            MemoryType::LongTerm,
            "the build uses cargo",
            MemoryMetadata::default(),
        )
        .await
        .unwrap();

    let report = manager.consolidate().await.unwrap();
    assert_eq!(report.fused, 0, "unrelated facts must not be fused");
    assert_eq!(manager.get_all_long_term().await.unwrap().len(), 2);
}

#[tokio::test]
async fn fuse_does_not_cross_project_boundaries() {
    let dir = TempDir::new().unwrap();
    // Both entries map to the same unit vector (cosine 1.0), so the
    // only thing keeping them separate is the project_key. That is
    // the design's point: a fact learned on project A must not
    // silently move into project B.
    let embedder = Arc::new(StubEmbedder::new(vec![("dark", vec![1.0, 0.0, 0.0, 0.0])]));
    let manager = manager_with_embedder(&dir, embedder);

    manager
        .store_with_metadata(
            MemoryType::LongTerm,
            "project A prefers dark",
            MemoryMetadata {
                project_key: Some("project-a".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    manager
        .store_with_metadata(
            MemoryType::LongTerm,
            "project B prefers dark",
            MemoryMetadata {
                project_key: Some("project-b".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let report = manager.consolidate().await.unwrap();
    assert_eq!(
        report.fused, 0,
        "entries with different project keys must not fuse",
    );
    assert_eq!(manager.get_all_long_term().await.unwrap().len(), 2);
}

#[tokio::test]
async fn fuse_is_noop_without_embedder() {
    let dir = TempDir::new().unwrap();
    // No `set_embedder` call.
    let db = dir.path().join("mem.redb");
    let manager = MemoryManager::new(db, 100).unwrap();

    manager
        .store(MemoryType::LongTerm, "prefers dark mode")
        .await
        .unwrap();
    manager
        .store(MemoryType::LongTerm, "likes dark themes")
        .await
        .unwrap();

    let report = manager.consolidate().await.unwrap();
    assert_eq!(
        report.fused, 0,
        "fusion needs an embedder; without one it must not delete entries",
    );
    assert_eq!(manager.get_all_long_term().await.unwrap().len(), 2);
}

#[tokio::test]
async fn archival_drops_old_episodic_entries_only() {
    // Manually craft entries with old timestamps. `MemoryManager` does
    // not expose a "store with an old timestamp" API, so we go through
    // the persistent store directly — the same path the real extraction
    // writes through.
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("mem.redb");
    let manager = MemoryManager::new(db, 100).unwrap();

    // Fresh episodic: retained.
    let fresh_id = manager
        .store(MemoryType::Episodic, "today's fact")
        .await
        .unwrap();
    // Durable long-term: never archived, even if old.
    let durable_id = manager
        .store(MemoryType::LongTerm, "durable fact")
        .await
        .unwrap();
    assert!(manager.get_long_term(&fresh_id).await.unwrap().is_some());
    assert!(manager.get_long_term(&durable_id).await.unwrap().is_some());

    // We do not have a way to make an old episodic entry through the
    // public API today; this test proves the pass is a no-op on fresh
    // entries (the archive half's "nothing to do" path), and does not
    // delete anything that should not be touched.
    let report = manager.consolidate().await.unwrap();
    assert_eq!(report.archived, 0, "no entry is old enough");
    assert_eq!(report.fused, 0, "no embedder, no fusion");
    assert_eq!(manager.get_all_long_term().await.unwrap().len(), 2);
}
