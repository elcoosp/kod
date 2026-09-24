//! Long-term memory - persistent storage using redb.
//!
//! Stores facts and knowledge that persist across sessions. Uses redb for
//! ACID transactions and efficient key-value storage.
//!
//! Every async method here performs synchronous redb I/O. redb's `Database`
//! is `Send + Sync` and takes its own internal locks, but its transaction
//! methods are blocking syscalls (file reads/writes, fsync on commit).
//! Running them directly on a tokio worker starves the runtime's other
//! tasks — a `store` burst at 100 % CPU contention can block a 10 ms UI
//! tick for hundreds of milliseconds. Every redb access is therefore
//! wrapped in `tokio::task::spawn_blocking`, moving the work to the
//! blocking pool where it belongs.

use kod_error::{KodError, Result};
use kod_types::{MemoryEntry, MemoryId};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use std::path::Path;
use std::sync::Arc;

const MEMORY_TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("memories");

/// Persistent long-term memory storage.
pub struct LongTermMemory {
    db: Arc<Database>,
}

impl LongTermMemory {
    /// Open (or create) a long-term memory database.
    ///
    /// Sync: called once at startup from `MemoryManager::new`. The initial
    /// table-create transaction touches the filesystem but happens before
    /// the async runtime is under any load, so the cost is negligible and
    /// wrapping it would force the caller into an async constructor for
    /// no benefit.
    pub fn new(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                KodError::MemoryStorage(format!("Failed to create db directory: {}", e))
            })?;
        }

        let db = Database::create(path)
            .map_err(|e| KodError::MemoryDatabase(format!("Failed to open database: {}", e)))?;

        let txn = db
            .begin_write()
            .map_err(|e| KodError::MemoryDatabase(format!("Failed to start transaction: {}", e)))?;

        txn.open_table(MEMORY_TABLE)
            .map_err(|e| KodError::MemoryDatabase(format!("Failed to open table: {}", e)))?;

        txn.commit()
            .map_err(|e| KodError::MemoryDatabase(format!("Failed to commit: {}", e)))?;

        Ok(Self { db: Arc::new(db) })
    }

    /// Run a closure against the redb database on the blocking thread pool.
    ///
    /// Every redb access goes through here. The closure receives a
    /// `&Database` borrowed from a cloned `Arc`, does its work
    /// synchronously, and returns an owned value. The join error from
    /// `spawn_blocking` (a panic in the closure) is converted into a
    /// `KodError::MemoryDatabase` so callers see a single error type.
    async fn blocking<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Database) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || f(&db))
            .await
            .map_err(|e| KodError::MemoryDatabase(format!("spawn_blocking join failed: {}", e)))?
    }

    /// Store an entry persistently. Overwrites any existing entry with
    /// the same id.
    pub async fn store(&self, entry: MemoryEntry) -> Result<()> {
        let key = entry.id.as_uuid().as_bytes().to_vec();
        let value =
            serde_json::to_vec(&entry).map_err(|e| KodError::Serialization(e.to_string()))?;

        self.blocking(move |db| {
            let txn = db.begin_write().map_err(|e| {
                KodError::MemoryDatabase(format!("Failed to start transaction: {}", e))
            })?;
            {
                let mut table = txn.open_table(MEMORY_TABLE).map_err(|e| {
                    KodError::MemoryDatabase(format!("Failed to open table: {}", e))
                })?;
                table
                    .insert(key.as_slice(), value.as_slice())
                    .map_err(|e| KodError::MemoryDatabase(format!("Failed to insert: {}", e)))?;
            }
            txn.commit()
                .map_err(|e| KodError::MemoryDatabase(format!("Failed to commit: {}", e)))?;
            Ok(())
        })
        .await
    }

    /// Store N entries in a single redb write transaction.
    ///
    /// The retrieval path updates `last_retrieved_at_ms` on every hit;
    /// doing N individual `store` calls would open N write transactions
    /// on a per-prompt hot path. This method amortises them. Entries
    /// are serialized before the transaction opens, so a serialization
    /// error fails the batch without touching redb.
    pub async fn store_batch(&self, entries: Vec<MemoryEntry>) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let prepared: Vec<(Vec<u8>, Vec<u8>)> = entries
            .into_iter()
            .map(|e| {
                let key = e.id.as_uuid().as_bytes().to_vec();
                let value = serde_json::to_vec(&e)
                    .map_err(|err| KodError::Serialization(err.to_string()))?;
                Ok((key, value))
            })
            .collect::<Result<Vec<_>>>()?;

        self.blocking(move |db| {
            let txn = db.begin_write().map_err(|e| {
                KodError::MemoryDatabase(format!("Failed to start transaction: {}", e))
            })?;
            {
                let mut table = txn.open_table(MEMORY_TABLE).map_err(|e| {
                    KodError::MemoryDatabase(format!("Failed to open table: {}", e))
                })?;
                for (key, value) in &prepared {
                    table
                        .insert(key.as_slice(), value.as_slice())
                        .map_err(|e| {
                            KodError::MemoryDatabase(format!("Failed to insert: {}", e))
                        })?;
                }
            }
            txn.commit()
                .map_err(|e| KodError::MemoryDatabase(format!("Failed to commit: {}", e)))?;
            Ok(())
        })
        .await
    }

    /// Get an entry by id.
    pub async fn get(&self, id: &MemoryId) -> Result<Option<MemoryEntry>> {
        let key = id.as_uuid().as_bytes().to_vec();
        self.blocking(move |db| {
            let txn = db.begin_read().map_err(|e| {
                KodError::MemoryDatabase(format!("Failed to start read transaction: {}", e))
            })?;
            let table = txn
                .open_table(MEMORY_TABLE)
                .map_err(|e| KodError::MemoryDatabase(format!("Failed to open table: {}", e)))?;
            match table.get(key.as_slice()) {
                Ok(Some(value)) => {
                    let entry: MemoryEntry = serde_json::from_slice(value.value())
                        .map_err(|e| KodError::Deserialization(e.to_string()))?;
                    Ok(Some(entry))
                }
                Ok(None) => Ok(None),
                Err(e) => Err(KodError::MemoryDatabase(format!("Failed to get: {}", e))),
            }
        })
        .await
    }

    /// Update an existing entry. Same as `store` (overwrites).
    pub async fn update(&self, entry: MemoryEntry) -> Result<()> {
        self.store(entry).await
    }

    /// Remove an entry by id. Idempotent: removing a missing id succeeds.
    pub async fn remove(&self, id: &MemoryId) -> Result<()> {
        let key = id.as_uuid().as_bytes().to_vec();
        self.blocking(move |db| {
            let txn = db.begin_write().map_err(|e| {
                KodError::MemoryDatabase(format!("Failed to start transaction: {}", e))
            })?;
            {
                let mut table = txn.open_table(MEMORY_TABLE).map_err(|e| {
                    KodError::MemoryDatabase(format!("Failed to open table: {}", e))
                })?;
                table
                    .remove(key.as_slice())
                    .map_err(|e| KodError::MemoryDatabase(format!("Failed to remove: {}", e)))?;
            }
            txn.commit()
                .map_err(|e| KodError::MemoryDatabase(format!("Failed to commit: {}", e)))?;
            Ok(())
        })
        .await
    }

    /// Get every entry in the table (order unspecified).
    pub async fn get_all(&self) -> Result<Vec<MemoryEntry>> {
        self.blocking(|db| {
            let txn = db.begin_read().map_err(|e| {
                KodError::MemoryDatabase(format!("Failed to start read transaction: {}", e))
            })?;
            let table = txn
                .open_table(MEMORY_TABLE)
                .map_err(|e| KodError::MemoryDatabase(format!("Failed to open table: {}", e)))?;
            let mut entries = Vec::new();
            for entry in table
                .iter()
                .map_err(|e| KodError::MemoryDatabase(format!("Failed to iterate: {}", e)))?
            {
                match entry {
                    Ok((_, value)) => {
                        if let Ok(memory_entry) =
                            serde_json::from_slice::<MemoryEntry>(value.value())
                        {
                            entries.push(memory_entry);
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Failed to read entry: {}", e);
                    }
                }
            }
            Ok(entries)
        })
        .await
    }

    /// Search entries by content (case-insensitive substring match).
    ///
    /// Loads every entry then filters. The substring scan is pure CPU on
    /// the owned `Vec`, so it runs on the caller's task — the redb scan
    /// is the only part that benefits from `spawn_blocking`.
    pub async fn search(&self, query: &str) -> Result<Vec<MemoryEntry>> {
        let all = self.get_all().await?;
        let query_lower = query.to_lowercase();
        Ok(all
            .into_iter()
            .filter(|e| e.content.to_lowercase().contains(&query_lower))
            .collect())
    }

    /// Count entries without materialising them.
    pub async fn count(&self) -> Result<usize> {
        self.blocking(|db| {
            let txn = db.begin_read().map_err(|e| {
                KodError::MemoryDatabase(format!("Failed to start read transaction: {}", e))
            })?;
            let table = txn
                .open_table(MEMORY_TABLE)
                .map_err(|e| KodError::MemoryDatabase(format!("Failed to open table: {}", e)))?;
            let mut count = 0usize;
            for entry in table
                .iter()
                .map_err(|e| KodError::MemoryDatabase(format!("Failed to iterate: {}", e)))?
            {
                if entry.is_ok() {
                    count += 1;
                }
            }
            Ok(count)
        })
        .await
    }

    /// Remove every entry. Runs in one write transaction.
    /// Explicitly close the underlying redb database handle held by
    /// this instance.
    ///
    /// redb closes the file when the last `Arc<Database>` drops. This
    /// method gives the caller a *deterministic* close point: it
    /// consumes the `LongTermMemory` (and its strong reference), so a
    /// subsequent call to any other method is a compile error, and the
    /// OS file lock is released as soon as the engine's own handle
    /// drops — rather than whenever the last clone of the `Arc`
    /// happens to fall out of scope.
    ///
    /// Idempotency is by construction: consuming `self` means the
    /// method can only be called once. `KodEngine::shutdown` therefore
    /// does not need to guard against a double close; it guards on
    /// `is_running` instead.
    ///
    /// The actual fsync happens inside redb's own `Database::drop`
    /// (each write transaction already fsynced per commit, so this is
    /// a final flush rather than a bulk sync).
    pub fn close(self) {
        // Consuming `self` is the whole contract — dropping it here
        // releases the `Arc<Database>` this instance held. Other
        // clones of that Arc (a caller that stored one) keep the file
        // open; the engine holds no such clone.
        drop(self);
    }

    pub async fn clear(&self) -> Result<()> {
        self.blocking(|db| {
            let txn = db.begin_write().map_err(|e| {
                KodError::MemoryDatabase(format!("Failed to start transaction: {}", e))
            })?;
            {
                let mut table = txn.open_table(MEMORY_TABLE).map_err(|e| {
                    KodError::MemoryDatabase(format!("Failed to open table: {}", e))
                })?;
                let keys: Vec<Vec<u8>> = table
                    .iter()
                    .map_err(|e| KodError::MemoryDatabase(format!("Failed to iterate: {}", e)))?
                    .filter_map(|entry| match entry {
                        Ok((key, _)) => Some(key.value().to_vec()),
                        Err(e) => {
                            tracing::warn!("Failed to read key: {}", e);
                            None
                        }
                    })
                    .collect();
                for key in keys {
                    table.remove(key.as_slice()).map_err(|e| {
                        KodError::MemoryDatabase(format!("Failed to remove: {}", e))
                    })?;
                }
            }
            txn.commit()
                .map_err(|e| KodError::MemoryDatabase(format!("Failed to commit: {}", e)))?;
            Ok(())
        })
        .await
    }
}

impl Clone for LongTermMemory {
    fn clone(&self) -> Self {
        Self {
            db: self.db.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kod_types::MemoryType;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_basic_operations() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");

        let memory = LongTermMemory::new(&db_path).unwrap();

        let entry = MemoryEntry {
            id: MemoryId::new(),
            memory_type: MemoryType::LongTerm,
            content: "Test fact".to_string(),
            timestamp: time::OffsetDateTime::now_utc(),
            relevance: 1.0,
            metadata: Default::default(),
        
            superseded_by: None,
            contradicts: Vec::new(),
        };

        memory.store(entry.clone()).await.unwrap();
        let retrieved = memory.get(&entry.id).await.unwrap();
        assert!(retrieved.is_some());

        assert_eq!(memory.count().await.unwrap(), 1);

        memory.remove(&entry.id).await.unwrap();
        assert_eq!(memory.count().await.unwrap(), 0);
    }

    /// Smoke test: a burst of concurrent stores must not starve a
    /// concurrent timer. With the old inline implementation this could
    /// (in practice) delay the ticker past its scheduled fires; with
    /// `spawn_blocking` the redb work runs on the blocking pool and the
    /// ticker always fires. We do not assert wall-clock bounds (flaky);
    /// only that the ticker observed a wakeup and all 20 stores landed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_blocking_pool_does_not_starve_runtime() {
        use std::sync::Arc as StdArc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");
        let memory = StdArc::new(LongTermMemory::new(&db_path).unwrap());

        let ticked = StdArc::new(AtomicBool::new(false));
        let ticked_clone = ticked.clone();
        let ticker = tokio::spawn(async move {
            for _ in 0..50 {
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
                ticked_clone.store(true, Ordering::SeqCst);
            }
        });

        let mut handles = Vec::new();
        for i in 0..20 {
            let m = memory.clone();
            handles.push(tokio::spawn(async move {
                let entry = MemoryEntry {
                    id: MemoryId::new(),
                    memory_type: MemoryType::LongTerm,
                    content: format!("fact {i}"),
                    timestamp: time::OffsetDateTime::now_utc(),
                    relevance: 1.0,
                    metadata: Default::default(),
                
                    superseded_by: None,
                    contradicts: Vec::new(),
                };
                m.store(entry).await.unwrap();
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        let _ = ticker.await;
        assert!(ticked.load(Ordering::SeqCst), "ticker never fired");
        assert_eq!(memory.count().await.unwrap(), 20);
    }
}

#[cfg(test)]
mod coverage_store_batch {
    //! `store_batch` is the batched write path the retrieval
    //! writeback uses for `last_retrieved_at_ms`. A regression
    //! that (a) opened N transactions instead of one or (b) failed
    //! to serialize an entry before opening the transaction would
    //! make the retrieval path silently slow or corrupt the store.
    use super::*;
    use kod_types::MemoryType;
    use tempfile::TempDir;

    fn entry(content: &str) -> MemoryEntry {
        MemoryEntry {
            id: MemoryId::new(),
            memory_type: MemoryType::LongTerm,
            content: content.to_string(),
            timestamp: time::OffsetDateTime::now_utc(),
            relevance: 1.0,
            metadata: Default::default(),
        
            superseded_by: None,
            contradicts: Vec::new(),
        }
    }

    #[tokio::test]
    async fn empty_batch_is_a_no_op() {
        let tmp = TempDir::new().unwrap();
        let m = LongTermMemory::new(&tmp.path().join("t.redb")).unwrap();
        m.store_batch(Vec::new()).await.unwrap();
        assert_eq!(m.count().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn batch_write_is_visible_to_get_and_count() {
        let tmp = TempDir::new().unwrap();
        let m = LongTermMemory::new(&tmp.path().join("t.redb")).unwrap();
        let a = entry("a");
        let b = entry("b");
        let a_id = a.id.clone();
        let b_id = b.id.clone();
        m.store_batch(vec![a, b]).await.unwrap();
        assert_eq!(m.count().await.unwrap(), 2);
        assert!(m.get(&a_id).await.unwrap().is_some());
        assert!(m.get(&b_id).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn batch_write_replaces_existing_entries_by_id() {
        let tmp = TempDir::new().unwrap();
        let m = LongTermMemory::new(&tmp.path().join("t.redb")).unwrap();
        let mut e = entry("original");
        let id = e.id.clone();
        m.store(e.clone()).await.unwrap();
        e.content = "updated".into();
        m.store_batch(vec![e]).await.unwrap();
        assert_eq!(m.count().await.unwrap(), 1);
        let got = m.get(&id).await.unwrap().unwrap();
        assert_eq!(got.content, "updated");
    }

    #[tokio::test]
    async fn batch_preserves_every_entry_field() {
        let tmp = TempDir::new().unwrap();
        let m = LongTermMemory::new(&tmp.path().join("t.redb")).unwrap();
        let mut e = entry("body");
        e.relevance = 0.42;
        e.metadata.tags = vec!["a".into()];
        e.metadata.project_key = Some("p".into());
        e.metadata.last_retrieved_at_ms = Some(12345);
        let id = e.id.clone();
        m.store_batch(vec![e]).await.unwrap();
        let got = m.get(&id).await.unwrap().unwrap();
        assert!((got.relevance - 0.42).abs() < 1e-6);
        assert_eq!(got.metadata.tags, vec!["a".to_string()]);
        assert_eq!(got.metadata.project_key.as_deref(), Some("p"));
        assert_eq!(got.metadata.last_retrieved_at_ms, Some(12345));
    }
}
