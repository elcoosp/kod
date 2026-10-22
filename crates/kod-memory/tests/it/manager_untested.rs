//! `MemoryManager`'s setters and short-term operations.
//!
//! `tests/it/` already covers store/retrieve/search/consolidation and
//! the embedder-panic isolation. The methods here are the setters
//! (`set_context_window`, `set_scorer`, `set_redactor`,
//! `disable_redaction`) and the short-term surface (`get_short_term`,
//! `get_all_short_term`, `compact`, `clear_short_term`, `clear_all`),
//! plus `close`.
//!
//! # Deliberately not covered
//!
//! - `set_embedder` — needs an `EmbeddingClient` stub; the existing
//!   `PanicEmbedder` in `embedder_panic.rs` shows the shape, but the
//!   "configured"/"unusable" branches of `embedder_name` are the
//!   follow-up round.
//! - `flush_embeddings` — with no embedder installed there is nothing
//!   in flight; it is exercised indirectly by `close` (which calls it).
//! - `PendingMemory::{snapshot, is_fresh, revalidate}` — a different
//!   type (`impl PendingMemory`), its own freshness contract, its own
//!   round.
use kod_memory::manager::MemoryManager;
use kod_types::{MemoryType, redact::Redactor};
use tempfile::TempDir;

fn manager_in(dir: &std::path::Path) -> MemoryManager {
    MemoryManager::new(dir.join("m.redb"), 100).expect("manager construction")
}

// ---- setters -------------------------------------------------------

/// Every setter is a one-line field assignment. The test applies them
/// all; the assertion is "no panic, and the manager is still usable".
/// A `store` after the setters proves the manager was not left in a
/// broken state by any of them.
#[tokio::test]
async fn setters_apply_and_the_manager_still_works() {
    let dir = TempDir::new().unwrap();
    let mut mgr = manager_in(dir.path());

    mgr.set_context_window(2048);
    mgr.set_scorer(Default::default());
    mgr.set_redactor(Some(Redactor::default()));
    mgr.disable_redaction();

    // A manager whose setters ran must still accept a store. If a
    // setter corrupted internal state, this would fail or panic.
    let _ = mgr
        .store(MemoryType::ShortTerm, "after setters")
        .await
        .expect("store after setters must succeed");
}

/// `set_redactor(None)` and `disable_redaction` both turn redaction
/// off. A secret-shaped string stores verbatim under both — the
/// write-path redactor is gone.
#[tokio::test]
async fn disable_redaction_stores_secrets_verbatim() {
    let dir = TempDir::new().unwrap();
    let mut mgr = manager_in(dir.path());
    mgr.disable_redaction();

    let secret = "sk-abcdef0123456789abcdef0123456789";
    let id = mgr
        .store(MemoryType::ShortTerm, secret)
        .await
        .expect("store");
    let entry = mgr.get_short_term(&id).expect("stored entry present");
    assert!(
        entry.content.contains("sk-abcdef"),
        "with redaction disabled the content must be stored verbatim, \
         got: {:?}",
        entry.content,
    );
}

// ---- embedder_name -------------------------------------------------

/// A fresh manager has no embedder: `embedder_name` is `"none"`.
#[test]
fn embedder_name_is_none_by_default() {
    let dir = TempDir::new().unwrap();
    let mgr = manager_in(dir.path());
    assert_eq!(mgr.embedder_name(), "none");
}

// ---- short-term reads ----------------------------------------------

/// `get_short_term` returns the entry that `store` produced, by id.
#[tokio::test]
async fn get_short_term_returns_the_stored_entry() {
    let dir = TempDir::new().unwrap();
    let mgr = manager_in(dir.path());

    let id = mgr
        .store(MemoryType::ShortTerm, "remember me")
        .await
        .expect("store");
    let entry = mgr.get_short_term(&id).expect("entry present");
    assert_eq!(entry.content, "remember me");
    assert_eq!(entry.id, id);

    // An id that was never stored returns None, not an error.
    let ghost = kod_types::MemoryId::new();
    assert!(mgr.get_short_term(&ghost).is_none());
}

/// `get_all_short_term` returns every stored short-term entry.
#[tokio::test]
async fn get_all_short_term_returns_every_entry() {
    let dir = TempDir::new().unwrap();
    let mgr = manager_in(dir.path());

    for c in ["one", "two", "three"] {
        mgr.store(MemoryType::ShortTerm, c).await.expect("store");
    }
    let all = mgr.get_all_short_term();
    assert_eq!(all.len(), 3, "three stores, three short-term entries");
    let contents: Vec<&str> = all.iter().map(|e| e.content.as_str()).collect();
    for want in ["one", "two", "three"] {
        assert!(contents.contains(&want), "missing {want} in {contents:?}");
    }
}

// ---- compact -------------------------------------------------------

/// `compact(target)` keeps at most `target` entries. Store five, ask
/// for two: two must remain, three must be reported removed.
#[tokio::test]
async fn compact_keeps_at_most_target_entries() {
    let dir = TempDir::new().unwrap();
    let mgr = manager_in(dir.path());

    for i in 0..5 {
        mgr.store(MemoryType::ShortTerm, &format!("entry-{i}"))
            .await
            .expect("store");
    }
    assert_eq!(mgr.get_all_short_term().len(), 5);

    let report = mgr.compact(2);
    assert_eq!(
        mgr.get_all_short_term().len(),
        2,
        "compact(2) must leave two entries",
    );
    assert_eq!(
        report.short_term_removed, 3,
        "three entries must have been removed",
    );
}

// ---- clear_short_term ----------------------------------------------

/// `clear_short_term` empties the working set but leaves long-term
/// entries alone.
#[tokio::test]
async fn clear_short_term_leaves_long_term_alone() {
    let dir = TempDir::new().unwrap();
    let mgr = manager_in(dir.path());

    mgr.store(MemoryType::ShortTerm, "short")
        .await
        .expect("store");
    mgr.store(MemoryType::LongTerm, "long")
        .await
        .expect("store");

    mgr.clear_short_term();

    assert!(
        mgr.get_all_short_term().is_empty(),
        "short-term must be empty after clear",
    );
    assert_eq!(
        mgr.get_all_long_term().await.expect("read long-term").len(),
        1,
        "long-term must be untouched by clear_short_term",
    );
}

// ---- clear_all -----------------------------------------------------

/// `clear_all` empties both tiers.
#[tokio::test]
async fn clear_all_empties_both_tiers() {
    let dir = TempDir::new().unwrap();
    let mgr = manager_in(dir.path());

    mgr.store(MemoryType::ShortTerm, "short")
        .await
        .expect("store");
    mgr.store(MemoryType::LongTerm, "long")
        .await
        .expect("store");

    mgr.clear_all().await.expect("clear_all");

    assert!(
        mgr.get_all_short_term().is_empty(),
        "short-term after clear_all"
    );
    assert!(
        mgr.get_all_long_term()
            .await
            .expect("read long-term")
            .is_empty(),
        "long-term after clear_all",
    );
}

// ---- close ---------------------------------------------------------

/// `close` consumes the manager and releases the redb handle. The
/// proof is that a fresh `MemoryManager::new` on the same path
/// succeeds — a leaked lock would fail with the "Database already
/// open" error the constructor maps.
#[tokio::test]
async fn close_releases_the_db_handle() {
    let dir = TempDir::new().unwrap();

    // Two independent `PathBuf`s to the same file: `MemoryManager::new`
    // takes `db_path: PathBuf` by value, and the reopen needs its own
    // path after the first handle consumed it.
    let mgr = MemoryManager::new(dir.path().join("m.redb"), 100).expect("manager");
    mgr.store(MemoryType::LongTerm, "persisted")
        .await
        .expect("store");
    mgr.close().await;

    // Reopen; a leaked lock would fail here.
    let reopened = MemoryManager::new(dir.path().join("m.redb"), 100)
        .expect("reopening after close must succeed — a leaked lock would fail");
    assert_eq!(
        reopened.get_all_long_term().await.expect("read").len(),
        1,
        "data must persist across close/reopen",
    );
}
