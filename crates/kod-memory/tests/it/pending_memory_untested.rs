//! `PendingMemory::{snapshot, is_fresh, revalidate}` — the freshness
//! contract that decides whether a retrieved snapshot can still be
//! injected into the model's context.
//!
//! `is_fresh` is a wall-clock check against `FRESHNESS_WINDOW_MS`
//! (120s). `revalidate` re-reads every captured id from the store and
//! drops the ones that are no longer active — a superseded or removed
//! entry must not be injected after it went stale.
use kod_memory::long_term::LongTermMemory;
use kod_memory::manager::PendingMemory;
use kod_types::memory::MemoryContext;
use kod_types::{MemoryEntry, MemoryId, MemoryType};
use tempfile::TempDir;
use time::OffsetDateTime;

fn entry(content: &str) -> MemoryEntry {
    MemoryEntry {
        id: MemoryId::new(),
        memory_type: MemoryType::LongTerm,
        content: content.to_string(),
        timestamp: OffsetDateTime::now_utc(),
        relevance: 0.8,
        metadata: Default::default(),
        superseded_by: None,
        contradicts: Vec::new(),
    }
}

fn empty_context() -> MemoryContext {
    MemoryContext {
        working_memory: Vec::new(),
        long_term: Vec::new(),
        total_tokens: 0,
    }
}

/// `snapshot` records the ids of every entry in both tiers and stamps
/// `computed_at_ms` with a non-zero wall-clock value.
#[test]
fn snapshot_records_ids_from_both_tiers() {
    let wm = entry("working");
    let lt = entry("long");
    let (wm_id, lt_id) = (wm.id.clone(), lt.id.clone());
    let snapshot = PendingMemory::snapshot(MemoryContext {
        working_memory: vec![wm],
        long_term: vec![lt],
        total_tokens: 10,
    });
    assert_eq!(snapshot.ids.len(), 2);
    assert!(snapshot.ids.contains(&wm_id));
    assert!(snapshot.ids.contains(&lt_id));
    assert!(snapshot.computed_at_ms > 0);
}

/// An empty context is fresh immediately after snapshotting.
#[test]
fn empty_snapshot_is_fresh() {
    let snapshot = PendingMemory::snapshot(empty_context());
    assert!(snapshot.ids.is_empty());
    assert!(snapshot.is_fresh());
}

/// A snapshot older than the 120s window is not fresh. Setting the
/// timestamp back directly beats sleeping two minutes.
#[test]
fn old_snapshot_is_stale() {
    let mut snapshot = PendingMemory::snapshot(empty_context());
    snapshot.computed_at_ms = snapshot.computed_at_ms.saturating_sub(180_000);
    assert!(!snapshot.is_fresh());
}

/// `revalidate` against an empty store drops every captured id and
/// returns the drop count.
#[tokio::test]
async fn revalidate_drops_ids_absent_from_the_store() {
    let tmp = TempDir::new().unwrap();
    let store = LongTermMemory::new(&tmp.path().join("lt.redb")).expect("long term store");
    let mut snapshot = PendingMemory::snapshot(MemoryContext {
        working_memory: vec![entry("gone wm")],
        long_term: vec![entry("gone lt")],
        total_tokens: 10,
    });
    let dropped = snapshot.revalidate(&store).await;
    assert_eq!(dropped, 2);
    assert!(snapshot.context.working_memory.is_empty());
    assert!(snapshot.context.long_term.is_empty());
}

/// `revalidate` keeps an entry that is still present in the store.
#[tokio::test]
async fn revalidate_keeps_a_present_entry() {
    let tmp = TempDir::new().unwrap();
    let store = LongTermMemory::new(&tmp.path().join("lt.redb")).expect("long term store");
    let kept = entry("still here");
    let kept_id = kept.id.clone();
    store.store(kept.clone()).await.expect("store");
    let mut snapshot = PendingMemory::snapshot(MemoryContext {
        working_memory: vec![kept],
        long_term: Vec::new(),
        total_tokens: 10,
    });
    let dropped = snapshot.revalidate(&store).await;
    assert_eq!(dropped, 0);
    assert_eq!(snapshot.context.working_memory.len(), 1);
    assert_eq!(snapshot.context.working_memory[0].id, kept_id);
}
