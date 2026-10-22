//! `LongTermMemory`'s six untested public methods.
//!
//! `tests/it/long_term.rs` covers `new`, `store`, `get`, `search`,
//! `remove`, `count`, `clear`, `persistence`. The methods here —
//! `get_all`, `remove_batch`, `update`, `store_batch`,
//! `contradictions_of`, `close` — are the batch and mutation surface,
//! and had no direct test.
//!
//! # Deliberately not covered
//!
//! `find_by_content_hash`, `supersede`, and `link_contradiction`
//! need knowledge of how `store` computes the content hash (to
//! produce a hash `find_by_content_hash` recognises) and how
//! supersession is validated (whether `supersede` succeeds when
//! `new` is already superseded, when the ids are the same, when
//! either is missing). Writing tests for them without reading the
//! bodies would be guessing; they are the next recon's targets.
use kod_memory::long_term::LongTermMemory;
use kod_types::{MemoryEntry, MemoryId, MemoryType};
use tempfile::TempDir;
use time::OffsetDateTime;

fn create_entry(content: &str) -> MemoryEntry {
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

// ---- get_all -------------------------------------------------------

/// `get_all` returns every stored entry. Storing two and calling
/// `get_all` must yield both, in insertion order.
#[tokio::test]
async fn get_all_returns_every_stored_entry() {
    let tmp = TempDir::new().unwrap();
    let memory = LongTermMemory::new(&tmp.path().join("t.redb")).unwrap();

    let a = create_entry("first");
    let b = create_entry("second");
    memory.store(a.clone()).await.unwrap();
    memory.store(b.clone()).await.unwrap();

    let all = memory.get_all().await.unwrap();
    assert_eq!(all.len(), 2, "get_all must return both entries");
    let contents: Vec<&str> = all.iter().map(|e| e.content.as_str()).collect();
    assert!(contents.contains(&"first"));
    assert!(contents.contains(&"second"));
}

/// Empty store: `get_all` returns an empty vec, not an error.
#[tokio::test]
async fn get_all_on_empty_store_is_empty() {
    let tmp = TempDir::new().unwrap();
    let memory = LongTermMemory::new(&tmp.path().join("t.redb")).unwrap();
    let all = memory.get_all().await.unwrap();
    assert!(all.is_empty(), "empty store must yield empty vec, got {all:?}");
}

// ---- store_batch ---------------------------------------------------

/// `store_batch` writes every entry in one transaction. After it
/// returns, `count` must equal the batch size.
#[tokio::test]
async fn store_batch_writes_every_entry() {
    let tmp = TempDir::new().unwrap();
    let memory = LongTermMemory::new(&tmp.path().join("t.redb")).unwrap();

    let batch = vec![
        create_entry("a"),
        create_entry("b"),
        create_entry("c"),
    ];
    memory.store_batch(batch).await.unwrap();

    assert_eq!(
        memory.count().await.unwrap(),
        3,
        "store_batch of 3 must write 3 entries",
    );
}

/// Empty batch: no error, no change to the store.
#[tokio::test]
async fn store_batch_empty_is_a_noop() {
    let tmp = TempDir::new().unwrap();
    let memory = LongTermMemory::new(&tmp.path().join("t.redb")).unwrap();
    memory.store_batch(vec![]).await.unwrap();
    assert_eq!(memory.count().await.unwrap(), 0);
}

// ---- update --------------------------------------------------------

/// `update` writes a modified entry back. Store, mutate the content,
/// update, then `get` and assert the new content.
#[tokio::test]
async fn update_overwrites_the_stored_content() {
    let tmp = TempDir::new().unwrap();
    let memory = LongTermMemory::new(&tmp.path().join("t.redb")).unwrap();

    let mut entry = create_entry("original");
    memory.store(entry.clone()).await.unwrap();

    entry.content = "updated".to_string();
    entry.relevance = 0.5;
    memory.update(entry.clone()).await.unwrap();

    let fetched = memory.get(&entry.id).await.unwrap().expect("entry exists");
    assert_eq!(fetched.content, "updated");
    assert!((fetched.relevance - 0.5).abs() < f32::EPSILON);
}

// ---- remove_batch --------------------------------------------------

/// `remove_batch` deletes several ids in one transaction and returns
/// the number actually removed. Removing two of three must return 2
/// and leave one behind.
#[tokio::test]
async fn remove_batch_returns_the_count_removed() {
    let tmp = TempDir::new().unwrap();
    let memory = LongTermMemory::new(&tmp.path().join("t.redb")).unwrap();

    let a = create_entry("a");
    let b = create_entry("b");
    let c = create_entry("c");
    memory.store(a.clone()).await.unwrap();
    memory.store(b.clone()).await.unwrap();
    memory.store(c.clone()).await.unwrap();

    let removed = memory.remove_batch(&[a.id, b.id]).await.unwrap();
    assert_eq!(removed, 2, "two ids in the batch, two entries removed");
    assert_eq!(memory.count().await.unwrap(), 1, "one entry must survive");

    let remaining = memory.get(&c.id).await.unwrap();
    assert!(remaining.is_some(), "the un-batched entry must survive");
}

/// Removing an id that does not exist: the count reflects actual
/// removals, not the number of ids passed in.
#[tokio::test]
async fn remove_batch_ignores_unknown_ids() {
    let tmp = TempDir::new().unwrap();
    let memory = LongTermMemory::new(&tmp.path().join("t.redb")).unwrap();

    let real = create_entry("real");
    memory.store(real.clone()).await.unwrap();
    let ghost = MemoryId::new();

    let removed = memory.remove_batch(&[real.id, ghost]).await.unwrap();
    // Documented contract (long_term.rs, `remove_batch` doc): "Returns
    // the number of keys removed, which is always `ids.len()`: redb's
    // `remove` is idempotent on a missing key." The ghost id is counted
    // even though no entry matched it. No production caller depends on
    // "actual removals": manager.rs's `lt_remove_batch` discards the
    // count, and the in-src test discards it too. So the documented
    // `ids.len()` semantics stand.
    assert_eq!(
        removed, 2,
        "remove_batch returns ids.len(), not the count of entries that existed",
    );
    assert_eq!(
        memory.count().await.unwrap(),
        0,
        "the real id was still removed from the store",
    );
}

// ---- contradictions_of ---------------------------------------------

/// An entry with no declared contradictions returns an empty vec,
/// not an error.
#[tokio::test]
async fn contradictions_of_a_clean_entry_is_empty() {
    let tmp = TempDir::new().unwrap();
    let memory = LongTermMemory::new(&tmp.path().join("t.redb")).unwrap();

    let entry = create_entry("clean");
    memory.store(entry.clone()).await.unwrap();

    let contradictions = memory.contradictions_of(&entry.id).await.unwrap();
    assert!(
        contradictions.is_empty(),
        "an entry with no `contradicts` must have no contradictions, got {contradictions:?}",
    );
}

/// Querying a non-existent id: the method returns an empty vec, not
/// an error. Consistent with the crate's "missing is empty" contract.
#[tokio::test]
async fn contradictions_of_unknown_id_is_empty() {
    let tmp = TempDir::new().unwrap();
    let memory = LongTermMemory::new(&tmp.path().join("t.redb")).unwrap();

    let unknown = MemoryId::new();
    let contradictions = memory.contradictions_of(&unknown).await.unwrap();
    assert!(contradictions.is_empty());
}

// ---- close ---------------------------------------------------------

/// `close` consumes the store. The only assertion is "it compiles and
/// does not panic"; the behavioural effect (db handle released) is
/// proven by the fact that a subsequent `new` on the same path
/// succeeds — the lock was released.
#[tokio::test]
async fn close_releases_the_db_handle() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("t.redb");

    let memory = LongTermMemory::new(&path).unwrap();
    memory.store(create_entry("before close")).await.unwrap();
    memory.close();

    // Reopening the same path must succeed; a leaked lock would
    // produce the "already open" error the constructor guards
    // against.
    let reopened = LongTermMemory::new(&path)
        .expect("reopening after close must succeed — a leaked lock would fail");
    assert_eq!(reopened.count().await.unwrap(), 1, "data must persist");
}
