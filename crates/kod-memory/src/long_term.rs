//! Long-term memory - persistent storage using redb.
//!
//! Stores facts and knowledge that persist across sessions.
//! Uses redb for ACID transactions and efficient key-value storage.

use kod_error::{KodError, Result};
use kod_types::{MemoryEntry, MemoryId};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use std::path::Path;
use std::sync::Arc;

const MEMORY_TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("memories");

/// Persistent long-term memory storage
pub struct LongTermMemory {
    db: Arc<Database>,
}

impl LongTermMemory {
    /// Open (or create) a long-term memory database
    pub fn new(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                KodError::MemoryStorage(format!("Failed to create db directory: {}", e))
            })?;
        }

        let db = Database::create(path)
            .map_err(|e| KodError::MemoryDatabase(format!("Failed to open database: {}", e)))?;

        // Create table if it doesn't exist
        let txn = db
            .begin_write()
            .map_err(|e| KodError::MemoryDatabase(format!("Failed to start transaction: {}", e)))?;

        txn.open_table(MEMORY_TABLE)
            .map_err(|e| KodError::MemoryDatabase(format!("Failed to open table: {}", e)))?;

        txn.commit()
            .map_err(|e| KodError::MemoryDatabase(format!("Failed to commit: {}", e)))?;

        Ok(Self { db: Arc::new(db) })
    }

    /// Store an entry persistently
    pub async fn store(&self, entry: MemoryEntry) -> Result<()> {
        let key = entry.id.as_uuid().as_bytes().to_vec();
        let value =
            serde_json::to_vec(&entry).map_err(|e| KodError::Serialization(e.to_string()))?;

        let txn = self
            .db
            .begin_write()
            .map_err(|e| KodError::MemoryDatabase(format!("Failed to start transaction: {}", e)))?;

        {
            let mut table = txn
                .open_table(MEMORY_TABLE)
                .map_err(|e| KodError::MemoryDatabase(format!("Failed to open table: {}", e)))?;

            table
                .insert(key.as_slice(), value.as_slice())
                .map_err(|e| KodError::MemoryDatabase(format!("Failed to insert: {}", e)))?;
        }

        txn.commit()
            .map_err(|e| KodError::MemoryDatabase(format!("Failed to commit: {}", e)))?;

        Ok(())
    }

    /// Get an entry by ID
    pub async fn get(&self, id: &MemoryId) -> Result<Option<MemoryEntry>> {
        let key = id.as_uuid().as_bytes().to_vec();

        let txn = self.db.begin_read().map_err(|e| {
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
    }

    /// Update an existing entry
    pub async fn update(&self, entry: MemoryEntry) -> Result<()> {
        // Update is same as store (overwrites)
        self.store(entry).await
    }

    /// Remove an entry
    pub async fn remove(&self, id: &MemoryId) -> Result<()> {
        let key = id.as_uuid().as_bytes().to_vec();

        let txn = self
            .db
            .begin_write()
            .map_err(|e| KodError::MemoryDatabase(format!("Failed to start transaction: {}", e)))?;

        {
            let mut table = txn
                .open_table(MEMORY_TABLE)
                .map_err(|e| KodError::MemoryDatabase(format!("Failed to open table: {}", e)))?;

            table
                .remove(key.as_slice())
                .map_err(|e| KodError::MemoryDatabase(format!("Failed to remove: {}", e)))?;
        }

        txn.commit()
            .map_err(|e| KodError::MemoryDatabase(format!("Failed to commit: {}", e)))?;

        Ok(())
    }

    /// Get all entries
    pub async fn get_all(&self) -> Result<Vec<MemoryEntry>> {
        let txn = self.db.begin_read().map_err(|e| {
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
                    if let Ok(memory_entry) = serde_json::from_slice::<MemoryEntry>(value.value()) {
                        entries.push(memory_entry);
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to read entry: {}", e);
                }
            }
        }

        Ok(entries)
    }

    /// Search entries by content (case-insensitive substring match)
    pub async fn search(&self, query: &str) -> Result<Vec<MemoryEntry>> {
        let all = self.get_all().await?;
        let query_lower = query.to_lowercase();

        Ok(all
            .into_iter()
            .filter(|e| e.content.to_lowercase().contains(&query_lower))
            .collect())
    }

    /// Count total entries
    pub async fn count(&self) -> Result<usize> {
        let txn = self.db.begin_read().map_err(|e| {
            KodError::MemoryDatabase(format!("Failed to start read transaction: {}", e))
        })?;

        let table = txn
            .open_table(MEMORY_TABLE)
            .map_err(|e| KodError::MemoryDatabase(format!("Failed to open table: {}", e)))?;

        let mut count = 0;
        for entry in table
            .iter()
            .map_err(|e| KodError::MemoryDatabase(format!("Failed to iterate: {}", e)))?
        {
            if entry.is_ok() {
                count += 1;
            }
        }

        Ok(count)
    }

    /// Clear all entries
    pub async fn clear(&self) -> Result<()> {
        let txn = self
            .db
            .begin_write()
            .map_err(|e| KodError::MemoryDatabase(format!("Failed to start transaction: {}", e)))?;

        {
            let mut table = txn
                .open_table(MEMORY_TABLE)
                .map_err(|e| KodError::MemoryDatabase(format!("Failed to open table: {}", e)))?;

            // Delete all entries by collecting keys first
            // In redb 4.x, the iterator yields (AccessGuard<K>, AccessGuard<V>)
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
                table
                    .remove(key.as_slice())
                    .map_err(|e| KodError::MemoryDatabase(format!("Failed to remove: {}", e)))?;
            }
        }

        txn.commit()
            .map_err(|e| KodError::MemoryDatabase(format!("Failed to commit: {}", e)))?;

        Ok(())
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
        };

        // Store and retrieve
        memory.store(entry.clone()).await.unwrap();
        let retrieved = memory.get(&entry.id).await.unwrap();
        assert!(retrieved.is_some());

        // Count
        assert_eq!(memory.count().await.unwrap(), 1);

        // Remove
        memory.remove(&entry.id).await.unwrap();
        assert_eq!(memory.count().await.unwrap(), 0);
    }
}
