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
use redb::{Database, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition};
use std::path::Path;
use std::sync::Arc;

const MEMORY_TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("memories");

/// F2i-10: `content_hash(8 bytes BE) || memory_type_tag(1) -> memory id`.
/// Lets the store path answer "is this exact content already present?"
/// without deserializing the whole `MEMORY_TABLE`. A miss falls back to
/// the full scan, so an entry written before this index existed still
/// dedups correctly.
const HASH_INDEX_TABLE: TableDefinition<&[u8], &[u8]> =
    TableDefinition::new("content_hash_index");

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
            .map_err(|e| {
                let s = e.to_string();
                if s.contains("locked") || s.contains("LockError") || s.contains("already") {
                    KodError::MemoryDatabase(format!(
                        "Failed to open memory database at {}: {} — another kod process may hold the file lock",
                        path.display(), e
                    ))
                } else {
                    KodError::MemoryDatabase(format!("Failed to open database: {}", e))
                }
            })?;

        let txn = db
            .begin_write()
            .map_err(|e| KodError::MemoryDatabase(format!("Failed to start transaction: {}", e)))?;

        txn.open_table(MEMORY_TABLE)
            .map_err(|e| KodError::MemoryDatabase(format!("Failed to open table: {}", e)))?;
        // F2i-10: create the index table on first open so a fresh db
        // has it; an existing db gets it the first time `store` writes.
        txn.open_table(HASH_INDEX_TABLE)
            .map_err(|e| KodError::MemoryDatabase(format!("Failed to open table: {}", e)))?;

        txn.commit()
            .map_err(|e| KodError::MemoryDatabase(format!("Failed to commit: {}", e)))?;

        let db = Arc::new(db);
        // T1-C12: one-time migration. If the hash index is empty but
        // the memory table is not, walk the memory table and populate
        // the index. After this, `find_by_content_hash` always
        // answers, and the O(N) fallback in `store_with_metadata`
        // never fires on a normal store.
        if let Err(e) = Self::migrate_index_if_stale(&db) {
            tracing::warn!(
                error = %e,
                "content-hash index migration failed; dedup will fall back \
                 to a full scan until the next clean open",
            );
        }
        Ok(Self { db })
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

    /// F2i-10: one byte standing in for `MemoryType`, so the index key
    /// is fixed-width without serde.
    fn type_tag(t: kod_types::MemoryType) -> u8 {
        match t {
            kod_types::MemoryType::ShortTerm => 0,
            kod_types::MemoryType::LongTerm => 1,
            kod_types::MemoryType::Episodic => 2,
        }
    }

    /// F2i-10: index key for `(content_hash, memory_type)`.
    fn index_key(hash: u64, t: kod_types::MemoryType) -> [u8; 9] {
        let mut k = [0u8; 9];
        k[..8].copy_from_slice(&hash.to_be_bytes());
        k[8] = Self::type_tag(t);
        k
    }

    /// F2i-10: look up an existing entry id by content hash +
    /// memory type, without deserializing the corpus. `None` means
    /// "no index hit" — the caller falls back to a full scan so an
    /// entry predating the index still dedups.
    pub async fn find_by_content_hash(
        &self,
        hash: u64,
        memory_type: kod_types::MemoryType,
    ) -> Result<Option<MemoryId>> {
        let key = Self::index_key(hash, memory_type);
        self.blocking(move |db| {
            let txn = db.begin_read().map_err(|e| {
                KodError::MemoryDatabase(format!("Failed to start read transaction: {}", e))
            })?;
            let table = match txn.open_table(HASH_INDEX_TABLE) {
                Ok(t) => t,
                // Table absent on an old db: no hit, caller rescans.
                Err(_) => return Ok(None),
            };
            let Some(v) = table
                .get(key.as_slice())
                .map_err(|e| KodError::MemoryDatabase(format!("Failed to get: {}", e)))?
            else {
                return Ok(None);
            };
            let bytes = v.value();
            if bytes.len() != 16 {
                return Ok(None);
            }
            let mut u = [0u8; 16];
            u.copy_from_slice(bytes);
            Ok(Some(MemoryId::from_uuid(uuid::Uuid::from_bytes(u))))
        })
        .await
    }

    /// T1-C12: one-time migration of the content-hash index.
    ///
    /// Entries written before the F2i-10 index existed have no index
    /// entry. Without this, every store for a fact that matches a
    /// pre-index entry falls through to an O(N) full-table scan in
    /// `store_with_metadata`. The migration walks the memory table
    /// once and populates the index for every entry; subsequent
    /// stores take the O(log n) path unconditionally.
    ///
    /// Idempotent: it returns early if the index already has any
    /// entries, so a fresh install and a migrated install both no-op
    /// on the second call.
    fn migrate_index_if_stale(db: &Arc<Database>) -> Result<()> {
        let db = Arc::clone(db);
        // T1-C12: `new()` is sync — the caller has not entered the
        // async runtime yet, or is on a current-thread runtime where
        // `block_in_place` would panic. Just do the redb work
        // synchronously; it is a one-time startup cost.
        {
            let read_txn = db.begin_read().map_err(|e| {
                KodError::MemoryDatabase(format!("migration read txn: {}", e))
            })?;
            let idx_table = match read_txn.open_table(HASH_INDEX_TABLE) {
                Ok(t) => t,
                Err(_) => return Ok(()), // no index table yet — nothing to migrate
            };
            let idx_len = idx_table.len().unwrap_or(0);
            let mem_table = read_txn.open_table(MEMORY_TABLE).map_err(|e| {
                KodError::MemoryDatabase(format!("migration mem table: {}", e))
            })?;
            let mem_len = mem_table.len().unwrap_or(0);
            drop(idx_table);
            drop(mem_table);
            drop(read_txn);
            if idx_len > 0 || mem_len == 0 {
                return Ok(());
            }
            tracing::info!(
                entries = mem_len,
                "content-hash index empty; migrating pre-index entries"
            );
            let write_txn = db.begin_write().map_err(|e| {
                KodError::MemoryDatabase(format!("migration write txn: {}", e))
            })?;
            let mut inserted = 0usize;
            {
                let mem = write_txn.open_table(MEMORY_TABLE).map_err(|e| {
                    KodError::MemoryDatabase(format!("migration open mem: {}", e))
                })?;
                let mut idx = write_txn.open_table(HASH_INDEX_TABLE).map_err(|e| {
                    KodError::MemoryDatabase(format!("migration open idx: {}", e))
                })?;
                let iter = mem.iter().map_err(|e| {
                    KodError::MemoryDatabase(format!("migration iter: {}", e))
                })?;
                for entry in iter {
                    let Ok((_k, v)) = entry else { continue };
                    let Ok(e) = serde_json::from_slice::<MemoryEntry>(v.value()) else {
                        continue;
                    };
                    use std::hash::{Hash, Hasher};
                    let mut h = std::collections::hash_map::DefaultHasher::new();
                    e.content.hash(&mut h);
                    let hash = h.finish();
                    let key = Self::index_key(hash, e.memory_type);
                    let val = e.id.as_uuid().as_bytes().to_vec();
                    if idx.insert(key.as_slice(), val.as_slice()).is_ok() {
                        inserted += 1;
                    }
                }
            }
            write_txn.commit().map_err(|e| {
                KodError::MemoryDatabase(format!("migration commit: {}", e))
            })?;
            tracing::info!(inserted, "content-hash index migrated");
            Ok(())
        }
    }


    /// Store an entry persistently. Overwrites any existing entry with
    /// the same id.
    pub async fn store(&self, entry: MemoryEntry) -> Result<()> {
        let key = entry.id.as_uuid().as_bytes().to_vec();
        let value =
            serde_json::to_vec(&entry).map_err(|e| KodError::Serialization(e.to_string()))?;
        // F2i-10: keep the content-hash index in step with the entry.
        let idx_key = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            entry.content.hash(&mut h);
            Self::index_key(h.finish(), entry.memory_type)
        };
        let idx_val = entry.id.as_uuid().as_bytes().to_vec();

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
                let mut idx = txn.open_table(HASH_INDEX_TABLE).map_err(|e| {
                    KodError::MemoryDatabase(format!("Failed to open table: {}", e))
                })?;
                idx.insert(idx_key.as_slice(), idx_val.as_slice())
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

    /// T1-C7: remove several ids in one write transaction. The
    /// per-id `remove` opens a fresh txn per call — N removals means
    /// N fsyncs, and a crash mid-loop leaves the store half-fused
    /// (survivor tags merged, duplicates still present, next
    /// consolidation pass re-fuses the same cluster).
    ///
    /// Returns the number of keys removed, which is always
    /// `ids.len()`: redb's `remove` is idempotent on a missing key.
    pub async fn remove_batch(&self, ids: &[MemoryId]) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        let keys: Vec<Vec<u8>> = ids
            .iter()
            .map(|id| id.as_uuid().as_bytes().to_vec())
            .collect();
        self.blocking(move |db| {
            let txn = db.begin_write().map_err(|e| {
                KodError::MemoryDatabase(format!("Failed to start transaction: {}", e))
            })?;
            {
                let mut table = txn.open_table(MEMORY_TABLE).map_err(|e| {
                    KodError::MemoryDatabase(format!("Failed to open table: {}", e))
                })?;
                for key in &keys {
                    let _ = table.remove(key.as_slice());
                }
            }
            txn.commit().map_err(|e| {
                KodError::MemoryDatabase(format!("Failed to commit: {}", e))
            })?;
            Ok(keys.len())
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
                        match serde_json::from_slice::<MemoryEntry>(value.value()) {
                            Ok(memory_entry) => entries.push(memory_entry),
                            Err(e) => tracing::warn!(
                                error = %e,
                                "get_all: corrupt entry skipped — schema migration may be needed"
                            ),
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
    /// Mark `old` as replaced by `new`.
    ///
    /// The old entry stays on disk — deleting it loses the audit
    /// trail of what was believed before — but stops appearing in
    /// retrieval. Both ids must exist; a caller superseding an entry
    /// that was already removed gets an error naming which one.
    pub async fn supersede(&self, old: &MemoryId, new: &MemoryId) -> Result<()> {
        let mut entry = self
            .get(old)
            .await?
            .ok_or_else(|| KodError::InvalidState(format!("no entry {old}")))?;
        if self.get(new).await?.is_none() {
            return Err(KodError::InvalidState(format!("no entry {new}")));
        }
        entry.superseded_by = Some(new.clone());
        self.update(entry).await
    }

    /// Record that two entries disagree.
    ///
    /// The link is symmetric: both entries carry each other's id, so a
    /// caller reading either side sees the disagreement. Both stay
    /// active — a contradiction is a fact to surface, not one to
    /// resolve by picking a winner.
    pub async fn link_contradiction(&self, a: &MemoryId, b: &MemoryId) -> Result<()> {
        let mut ea = self
            .get(a)
            .await?
            .ok_or_else(|| KodError::InvalidState(format!("no entry {a}")))?;
        let mut eb = self
            .get(b)
            .await?
            .ok_or_else(|| KodError::InvalidState(format!("no entry {b}")))?;
        if !ea.contradicts.contains(b) {
            ea.contradicts.push(b.clone());
            self.update(ea).await?;
        }
        if !eb.contradicts.contains(a) {
            eb.contradicts.push(a.clone());
            self.update(eb).await?;
        }
        Ok(())
    }

    /// Entries `id` contradicts, for a caller surfacing the
    /// disagreement.
    pub async fn contradictions_of(&self, id: &MemoryId) -> Result<Vec<MemoryEntry>> {
        let Some(entry) = self.get(id).await? else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for other in &entry.contradicts {
            if let Some(e) = self.get(other).await? {
                out.push(e);
            }
        }
        Ok(out)
    }

    pub async fn count(&self) -> Result<usize> {
        // T1-H3: redb tracks the row count internally; `len()` is an
        // O(1) B-tree count, not a walk. The pre-fix shape iterated
        // every entry.
        self.blocking(|db| {
            let txn = db.begin_read().map_err(|e| {
                KodError::MemoryDatabase(format!("Failed to start transaction: {}", e))
            })?;
            let table = txn.open_table(MEMORY_TABLE).map_err(|e| {
                KodError::MemoryDatabase(format!("Failed to open table: {}", e))
            })?;
            let n = table.len().map_err(|e| {
                KodError::MemoryDatabase(format!("Failed to read table len: {}", e))
            })?;
            Ok(n as usize)
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

    fn entry(content: &str) -> MemoryEntry {
        MemoryEntry {
            id: MemoryId::new(),
            memory_type: kod_types::MemoryType::LongTerm,
            content: content.to_string(),
            timestamp: time::OffsetDateTime::now_utc(),
            relevance: 1.0,
            metadata: Default::default(),
            superseded_by: None,
            contradicts: Vec::new(),
        }
    }

    #[tokio::test]
    async fn content_hash_index_dedups_without_a_scan() {
        // F2i-10: storing the same content twice must return the same
        // id via the index, not via a full-table scan.
        let tmp = tempfile::TempDir::new().unwrap();
        let m = LongTermMemory::new(&tmp.path().join("m.redb")).unwrap();
        let stored = entry("Rust is a systems language");
        let id = stored.id.clone();
        m.store(stored.clone()).await.unwrap();
        let hit = m
            .find_by_content_hash({
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                stored.content.hash(&mut h);
                h.finish()
            }, stored.memory_type)
            .await
            .unwrap();
        assert_eq!(hit, Some(id), "index must return the stored id");
    }

    #[tokio::test]
    async fn content_hash_index_miss_for_unknown() {
        let tmp = tempfile::TempDir::new().unwrap();
        let m = LongTermMemory::new(&tmp.path().join("m.redb")).unwrap();
        let hit = m
            .find_by_content_hash(0xdead_beef, kod_types::MemoryType::LongTerm)
            .await
            .unwrap();
        assert_eq!(hit, None);
    }

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

#[cfg(test)]
mod supersession_tests {
    use super::*;
    use kod_types::{MemoryEntry, MemoryId, MemoryMetadata, MemoryType};
    use time::OffsetDateTime;

    fn entry(content: &str) -> MemoryEntry {
        MemoryEntry {
            id: MemoryId::new(),
            memory_type: MemoryType::LongTerm,
            content: content.to_string(),
            timestamp: OffsetDateTime::now_utc(),
            relevance: 1.0,
            metadata: MemoryMetadata::default(),
            superseded_by: None,
            contradicts: Vec::new(),
        }
    }

    fn fixture() -> (tempfile::TempDir, LongTermMemory) {
        let tmp = tempfile::TempDir::new().unwrap();
        let db = LongTermMemory::new(&tmp.path().join("mem.redb")).unwrap();
        (tmp, db)
    }

    #[tokio::test]
    async fn supersede_marks_the_old_entry() {
        let (_tmp, s) = fixture();
        let old = entry("the user prefers tabs");
        let new = entry("the user prefers spaces");
        let (old_id, new_id) = (old.id.clone(), new.id.clone());
        s.store(old).await.unwrap();
        s.store(new).await.unwrap();

        s.supersede(&old_id, &new_id).await.unwrap();
        let got = s.get(&old_id).await.unwrap().unwrap();
        assert_eq!(got.superseded_by.as_ref(), Some(&new_id));
        assert!(!got.is_active(), "a superseded entry is inactive");
    }

    #[tokio::test]
    async fn supersede_with_a_missing_replacement_errors() {
        let (_tmp, s) = fixture();
        let old = entry("x");
        let old_id = old.id.clone();
        s.store(old).await.unwrap();
        let ghost = MemoryId::new();
        assert!(s.supersede(&old_id, &ghost).await.is_err());
    }

    #[tokio::test]
    async fn link_contradiction_is_symmetric() {
        let (_tmp, s) = fixture();
        let a = entry("the build uses cargo");
        let b = entry("the build uses make");
        let (aid, bid) = (a.id.clone(), b.id.clone());
        s.store(a).await.unwrap();
        s.store(b).await.unwrap();

        s.link_contradiction(&aid, &bid).await.unwrap();
        let ga = s.get(&aid).await.unwrap().unwrap();
        let gb = s.get(&bid).await.unwrap().unwrap();
        assert!(ga.contradicts.contains(&bid));
        assert!(gb.contradicts.contains(&aid), "the link is symmetric");
        assert!(ga.is_active(), "a contradicted entry stays active");
    }

    #[tokio::test]
    async fn linking_twice_does_not_duplicate() {
        let (_tmp, s) = fixture();
        let a = entry("a");
        let b = entry("b");
        let (aid, bid) = (a.id.clone(), b.id.clone());
        s.store(a).await.unwrap();
        s.store(b).await.unwrap();
        s.link_contradiction(&aid, &bid).await.unwrap();
        s.link_contradiction(&aid, &bid).await.unwrap();
        let ga = s.get(&aid).await.unwrap().unwrap();
        assert_eq!(ga.contradicts.len(), 1);
    }

    #[tokio::test]
    async fn contradictions_of_returns_the_peers() {
        let (_tmp, s) = fixture();
        let a = entry("a");
        let b = entry("b");
        let (aid, bid) = (a.id.clone(), b.id.clone());
        s.store(a).await.unwrap();
        s.store(b).await.unwrap();
        s.link_contradiction(&aid, &bid).await.unwrap();

        let peers = s.contradictions_of(&aid).await.unwrap();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].id, bid);
    }

    #[tokio::test]
    async fn count_matches_len_after_stores() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = LongTermMemory::new(&dir.path().join("lt.redb")).unwrap();
        assert_eq!(store.count().await.unwrap(), 0);
        for _ in 0..5 {
            store
                .store(MemoryEntry {
                    id: MemoryId::new(),
                    memory_type: MemoryType::LongTerm,
                    content: "x".into(),
                    timestamp: time::OffsetDateTime::now_utc(),
                    relevance: 1.0,
                    metadata: Default::default(),
                    superseded_by: None,
                    contradicts: Vec::new(),
                })
                .await
                .unwrap();
        }
        assert_eq!(store.count().await.unwrap(), 5);
        assert_eq!(store.count().await.unwrap(), store.get_all().await.unwrap().len());
    }
}
