# Chunk 4: Memory System Implementation

## Task 18: Short-Term Memory (In-Memory)

**Files:**
- Modify: `crates/kod-memory/Cargo.toml`
- Create: `crates/kod-memory/src/lib.rs`
- Create: `crates/kod-memory/src/short_term.rs`
- Test: `crates/kod-memory/tests/short_term.rs`

- [ ] **Step 1: Update kod-memory Cargo.toml**

```toml
[package]
name = "kod-memory"
version.workspace = true
edition.workspace = true
license.workspace = true
authors.workspace = true

[dependencies]
serde = { workspace = true }
serde_json = { workspace = true }
tokio = { workspace = true }
tracing = { workspace = true }
parking_lot = { workspace = true }
redb = { workspace = true }
time = { workspace = true }
kod-types = { path = "../kod-types" }
kod-error = { path = "../kod-error" }
kod-config = { path = "../kod-config" }

[dev-dependencies]
rstest = { workspace = true }
tempfile = "3.8"
```

- [ ] **Step 2: Write failing test for short-term memory**

Create `crates/kod-memory/tests/short_term.rs`:

```rust
use kod_memory::short_term::ShortTermMemory;
use kod_types::{MemoryEntry, MemoryId, MemoryType};
use time::OffsetDateTime;

fn create_entry(content: &str) -> MemoryEntry {
    MemoryEntry {
        id: MemoryId::new(),
        memory_type: MemoryType::ShortTerm,
        content: content.to_string(),
        timestamp: OffsetDateTime::now_utc(),
        relevance: 1.0,
        metadata: Default::default(),
    }
}

#[test]
fn test_store_and_retrieve() {
    let mut memory = ShortTermMemory::new(100);
    let entry = create_entry("User prefers dark mode");
    
    memory.store(entry.clone());
    
    let retrieved = memory.get(&entry.id);
    assert!(retrieved.is_some());
    assert_eq!(retrieved.unwrap().content, "User prefers dark mode");
}

#[test]
fn test_capacity_limit() {
    let mut memory = ShortTermMemory::new(3);
    
    // Store 5 entries in a capacity-3 memory
    for i in 0..5 {
        let entry = create_entry(&format!("Entry {}", i));
        memory.store(entry);
    }
    
    // Should only have 3 entries (oldest evicted)
    assert_eq!(memory.len(), 3);
    
    // The first two entries should be evicted
    let all = memory.get_all();
    let contents: Vec<String> = all.iter().map(|e| e.content.clone()).collect();
    
    assert!(!contents.contains(&"Entry 0".to_string()));
    assert!(!contents.contains(&"Entry 1".to_string()));
    assert!(contents.contains(&"Entry 2".to_string()));
    assert!(contents.contains(&"Entry 3".to_string()));
    assert!(contents.contains(&"Entry 4".to_string()));
}

#[test]
fn test_get_recent() {
    let mut memory = ShortTermMemory::new(100);
    
    for i in 0..10 {
        let entry = create_entry(&format!("Entry {}", i));
        memory.store(entry);
    }
    
    let recent = memory.get_recent(3);
    assert_eq!(recent.len(), 3);
    
    // Should get the most recent entries
    assert_eq!(recent[0].content, "Entry 7");
    assert_eq!(recent[1].content, "Entry 8");
    assert_eq!(recent[2].content, "Entry 9");
}

#[test]
fn test_remove_entry() {
    let mut memory = ShortTermMemory::new(100);
    let entry = create_entry("To be removed");
    
    memory.store(entry.clone());
    assert_eq!(memory.len(), 1);
    
    memory.remove(&entry.id);
    assert_eq!(memory.len(), 0);
    
    let retrieved = memory.get(&entry.id);
    assert!(retrieved.is_none());
}

#[test]
fn test_clear() {
    let mut memory = ShortTermMemory::new(100);
    
    for i in 0..5 {
        let entry = create_entry(&format!("Entry {}", i));
        memory.store(entry);
    }
    
    assert_eq!(memory.len(), 5);
    memory.clear();
    assert_eq!(memory.len(), 0);
}

#[test]
fn test_search_content() {
    let mut memory = ShortTermMemory::new(100);
    
    memory.store(create_entry("User likes Rust programming"));
    memory.store(create_entry("User has a meeting tomorrow"));
    memory.store(create_entry("Project uses Rust and Tokio"));
    
    let results = memory.search("Rust");
    assert_eq!(results.len(), 2);
    
    let contents: Vec<String> = results.iter().map(|e| e.content.clone()).collect();
    assert!(contents.iter().any(|c| c.contains("Rust programming")));
    assert!(contents.iter().any(|c| c.contains("Rust and Tokio")));
}

#[test]
fn test_fifo_eviction_order() {
    let mut memory = ShortTermMemory::new(2);
    
    // Store entries with slight time differences
    let entry1 = create_entry("First");
    std::thread::sleep(std::time::Duration::from_millis(10));
    let entry2 = create_entry("Second");
    std::thread::sleep(std::time::Duration::from_millis(10));
    let entry3 = create_entry("Third");
    
    memory.store(entry1);
    memory.store(entry2);
    memory.store(entry3); // This should evict "First"
    
    let all = memory.get_all();
    assert_eq!(all.len(), 2);
    
    // FIFO: first in, first out
    assert_eq!(all[0].content, "Second");
    assert_eq!(all[1].content, "Third");
}
```

- [ ] **Step 3: Run tests to verify they fail**

```bash
cargo test -p kod-memory --test short_term
```

Expected: FAIL - short_term module not implemented

- [ ] **Step 4: Implement short-term memory**

Create `crates/kod-memory/src/lib.rs`:

```rust
//! Memory system for multi-layer storage (short-term, long-term, episodic).
//!
//! This crate provides different memory backends with a unified interface
//! for storing and retrieving context.

pub mod short_term;
pub mod long_term;
pub mod episodic;
pub mod manager;
pub mod context;

pub use short_term::ShortTermMemory;
pub use long_term::LongTermMemory;
pub use episodic::EpisodicMemory;
pub use manager::MemoryManager;
```

Create `crates/kod-memory/src/short_term.rs`:

```rust
//! Short-term memory - in-memory storage with capacity limits.
//!
//! Uses FIFO eviction when capacity is exceeded. Suitable for
//! session-scoped context that doesn't need persistence.

use kod_types::{MemoryEntry, MemoryId};
use std::collections::HashMap;
use parking_lot::RwLock;

/// In-memory short-term storage with FIFO eviction
#[derive(Debug)]
pub struct ShortTermMemory {
    entries: RwLock<Vec<MemoryEntry>>,
    index: RwLock<HashMap<MemoryId, usize>>,
    capacity: usize,
}

impl ShortTermMemory {
    /// Create a new short-term memory with the given capacity
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: RwLock::new(Vec::with_capacity(capacity)),
            index: RwLock::new(HashMap::with_capacity(capacity)),
            capacity,
        }
    }

    /// Store an entry (evicts oldest if at capacity)
    pub fn store(&self, entry: MemoryEntry) {
        let mut entries = self.entries.write();
        
        // Check capacity and evict if necessary
        if entries.len() >= self.capacity {
            self.evict_oldest(&mut entries);
        }
        
        // Store entry
        let position = entries.len();
        entries.push(entry.clone());
        
        // Update index
        self.index.write().insert(entry.id, position);
    }

    /// Get an entry by ID
    pub fn get(&self, id: &MemoryId) -> Option<MemoryEntry> {
        let index = self.index.read();
        let position = index.get(id)?;
        
        let entries = self.entries.read();
        entries.get(*position).cloned()
    }

    /// Remove an entry by ID
    pub fn remove(&self, id: &MemoryId) -> Option<MemoryEntry> {
        let mut index = self.index.write();
        let position = index.remove(id)?;
        
        let mut entries = self.entries.write();
        
        if position < entries.len() {
            let removed = entries.remove(position);
            
            // Update index positions for entries after the removed one
            for entry in entries.iter().skip(position) {
                if let Some(pos) = index.get_mut(&entry.id) {
                    *pos -= 1;
                }
            }
            
            Some(removed)
        } else {
            None
        }
    }

    /// Get the N most recent entries
    pub fn get_recent(&self, count: usize) -> Vec<MemoryEntry> {
        let entries = self.entries.read();
        
        if entries.len() <= count {
            entries.clone()
        } else {
            entries[entries.len() - count..].to_vec()
        }
    }

    /// Get all entries
    pub fn get_all(&self) -> Vec<MemoryEntry> {
        self.entries.read().clone()
    }

    /// Search entries by content (case-insensitive substring match)
    pub fn search(&self, query: &str) -> Vec<MemoryEntry> {
        let query_lower = query.to_lowercase();
        let entries = self.entries.read();
        
        entries.iter()
            .filter(|e| e.content.to_lowercase().contains(&query_lower))
            .cloned()
            .collect()
    }

    /// Clear all entries
    pub fn clear(&self) {
        self.entries.write().clear();
        self.index.write().clear();
    }

    /// Get current number of entries
    pub fn len(&self) -> usize {
        self.entries.read().len()
    }

    /// Check if memory is empty
    pub fn is_empty(&self) -> bool {
        self.entries.read().is_empty()
    }

    /// Get capacity
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Evict the oldest entry (FIFO)
    fn evict_oldest(&self, entries: &mut Vec<MemoryEntry>) {
        if let Some(oldest) = entries.first() {
            self.index.write().remove(&oldest.id);
            entries.remove(0);
            
            // Update positions in index
            let mut index = self.index.write();
            for entry in entries.iter() {
                if let Some(pos) = index.get_mut(&entry.id) {
                    *pos = pos.saturating_sub(1);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kod_types::MemoryType;
    use time::OffsetDateTime;

    fn create_entry(content: &str) -> MemoryEntry {
        MemoryEntry {
            id: MemoryId::new(),
            memory_type: MemoryType::ShortTerm,
            content: content.to_string(),
            timestamp: OffsetDateTime::now_utc(),
            relevance: 1.0,
            metadata: Default::default(),
        }
    }

    #[test]
    fn test_store_and_get() {
        let memory = ShortTermMemory::new(10);
        let entry = create_entry("test");
        
        memory.store(entry.clone());
        
        let retrieved = memory.get(&entry.id);
        assert!(retrieved.is_some());
    }

    #[test]
    fn test_eviction() {
        let memory = ShortTermMemory::new(2);
        
        memory.store(create_entry("first"));
        memory.store(create_entry("second"));
        memory.store(create_entry("third")); // Should evict "first"
        
        assert_eq!(memory.len(), 2);
        
        let all = memory.get_all();
        assert_eq!(all[0].content, "second");
        assert_eq!(all[1].content, "third");
    }
}
```

- [ ] **Step 5: Run tests to verify they pass**

```bash
cargo test -p kod-memory --test short_term
cargo test -p kod-memory --lib short_term
```

Expected: All tests pass

- [ ] **Step 6: Commit**

```bash
git add crates/kod-memory/
git commit -m "feat(memory): add short-term memory with FIFO eviction"
```

---

## Task 19: Long-Term Memory (redb Persistent Storage)

**Files:**
- Create: `crates/kod-memory/src/long_term.rs`
- Test: `crates/kod-memory/tests/long_term.rs`

- [ ] **Step 1: Write failing test for long-term memory**

Create `crates/kod-memory/tests/long_term.rs`:

```rust
use kod_memory::long_term::LongTermMemory;
use kod_types::{MemoryEntry, MemoryId, MemoryType};
use tempfile::TempDir;

fn create_entry(content: &str) -> MemoryEntry {
    MemoryEntry {
        id: MemoryId::new(),
        memory_type: MemoryType::LongTerm,
        content: content.to_string(),
        timestamp: time::OffsetDateTime::now_utc(),
        relevance: 0.8,
        metadata: Default::default(),
    }
}

fn create_test_db() -> (LongTermMemory, TempDir) {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("test.redb");
    
    let memory = LongTermMemory::new(&db_path).unwrap();
    (memory, temp_dir)
}

#[tokio::test]
async fn test_store_and_retrieve() {
    let (mut memory, _temp) = create_test_db();
    let entry = create_entry("User prefers dark mode");
    
    memory.store(entry.clone()).await.unwrap();
    
    let retrieved = memory.get(&entry.id).await.unwrap();
    assert!(retrieved.is_some());
    assert_eq!(retrieved.unwrap().content, "User prefers dark mode");
}

#[tokio::test]
async fn test_persistence() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("test.redb");
    
    // Store an entry
    {
        let mut memory = LongTermMemory::new(&db_path).unwrap();
        let entry = create_entry("Persistent fact");
        memory.store(entry.clone()).await.unwrap();
    }
    
    // Create new instance and retrieve
    {
        let memory = LongTermMemory::new(&db_path).unwrap();
        let all = memory.get_all().await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].content, "Persistent fact");
    }
}

#[tokio::test]
async fn test_remove() {
    let (mut memory, _temp) = create_test_db();
    let entry = create_entry("To be removed");
    
    memory.store(entry.clone()).await.unwrap();
    assert_eq!(memory.count().await.unwrap(), 1);
    
    memory.remove(&entry.id).await.unwrap();
    assert_eq!(memory.count().await.unwrap(), 0);
    
    let retrieved = memory.get(&entry.id).await.unwrap();
    assert!(retrieved.is_none());
}

#[tokio::test]
async fn test_search_by_content() {
    let (mut memory, _temp) = create_test_db();
    
    memory.store(create_entry("User knows Rust programming")).await.unwrap();
    memory.store(create_entry("User has a dog named Rex")).await.unwrap();
    memory.store(create_entry("Project uses Rust and Tokio")).await.unwrap();
    
    let results = memory.search("Rust").await.unwrap();
    assert_eq!(results.len(), 2);
    
    let contents: Vec<String> = results.iter().map(|e| e.content.clone()).collect();
    assert!(contents.iter().any(|c| c.contains("Rust programming")));
    assert!(contents.iter().any(|c| c.contains("Rust and Tokio")));
}

#[tokio::test]
async fn test_search_case_insensitive() {
    let (mut memory, _temp) = create_test_db();
    
    memory.store(create_entry("The user LIKES rust")).await.unwrap();
    
    let results = memory.search("rust").await.unwrap();
    assert_eq!(results.len(), 1);
    
    let results = memory.search("RUST").await.unwrap();
    assert_eq!(results.len(), 1);
}

#[tokio::test]
async fn test_get_all() {
    let (mut memory, _temp) = create_test_db();
    
    for i in 0..5 {
        memory.store(create_entry(&format!("Fact {}", i))).await.unwrap();
    }
    
    let all = memory.get_all().await.unwrap();
    assert_eq!(all.len(), 5);
}

#[tokio::test]
async fn test_update_entry() {
    let (mut memory, _temp) = create_test_db();
    let mut entry = create_entry("Original content");
    
    memory.store(entry.clone()).await.unwrap();
    
    // Update the entry
    entry.content = "Updated content".to_string();
    memory.update(entry.clone()).await.unwrap();
    
    let retrieved = memory.get(&entry.id).await.unwrap();
    assert_eq!(retrieved.unwrap().content, "Updated content");
}

#[tokio::test]
async fn test_count() {
    let (mut memory, _temp) = create_test_db();
    
    assert_eq!(memory.count().await.unwrap(), 0);
    
    memory.store(create_entry("Fact 1")).await.unwrap();
    memory.store(create_entry("Fact 2")).await.unwrap();
    
    assert_eq!(memory.count().await.unwrap(), 2);
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test -p kod-memory --test long_term
```

Expected: FAIL - long_term module not implemented

- [ ] **Step 3: Implement long-term memory with redb**

Create `crates/kod-memory/src/long_term.rs`:

```rust
//! Long-term memory - persistent storage using redb.
//!
//! Stores facts and knowledge that persist across sessions.
//! Uses redb for ACID transactions and efficient key-value storage.

use kod_error::{KodError, Result};
use kod_types::{MemoryEntry, MemoryId};
use redb::{Database, ReadableTable, TableDefinition};
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
            std::fs::create_dir_all(parent)
                .map_err(|e| KodError::MemoryStorage(format!("Failed to create db directory: {}", e)))?;
        }
        
        let db = Database::create(path)
            .map_err(|e| KodError::MemoryDatabase(format!("Failed to open database: {}", e)))?;
        
        // Create table if it doesn't exist
        let txn = db.begin_write()
            .map_err(|e| KodError::MemoryDatabase(format!("Failed to start transaction: {}", e)))?;
        
        txn.open_table(MEMORY_TABLE)
            .map_err(|e| KodError::MemoryDatabase(format!("Failed to open table: {}", e)))?;
        
        txn.commit()
            .map_err(|e| KodError::MemoryDatabase(format!("Failed to commit: {}", e)))?;
        
        Ok(Self {
            db: Arc::new(db),
        })
    }

    /// Store an entry persistently
    pub async fn store(&self, entry: MemoryEntry) -> Result<()> {
        let key = entry.id.as_uuid().as_bytes().to_vec();
        let value = serde_json::to_vec(&entry)
            .map_err(|e| KodError::Serialization(e.to_string()))?;
        
        let txn = self.db.begin_write()
            .map_err(|e| KodError::MemoryDatabase(format!("Failed to start transaction: {}", e)))?;
        
        {
            let mut table = txn.open_table(MEMORY_TABLE)
                .map_err(|e| KodError::MemoryDatabase(format!("Failed to open table: {}", e)))?;
            
            table.insert(
                key.as_slice(),
                value.as_slice(),
            ).map_err(|e| KodError::MemoryDatabase(format!("Failed to insert: {}", e)))?;
        }
        
        txn.commit()
            .map_err(|e| KodError::MemoryDatabase(format!("Failed to commit: {}", e)))?;
        
        Ok(())
    }

    /// Get an entry by ID
    pub async fn get(&self, id: &MemoryId) -> Result<Option<MemoryEntry>> {
        let key = id.as_uuid().as_bytes().to_vec();
        
        let txn = self.db.begin_read()
            .map_err(|e| KodError::MemoryDatabase(format!("Failed to start read transaction: {}", e)))?;
        
        let table = txn.open_table(MEMORY_TABLE)
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
        
        let txn = self.db.begin_write()
            .map_err(|e| KodError::MemoryDatabase(format!("Failed to start transaction: {}", e)))?;
        
        {
            let mut table = txn.open_table(MEMORY_TABLE)
                .map_err(|e| KodError::MemoryDatabase(format!("Failed to open table: {}", e)))?;
            
            table.remove(key.as_slice())
                .map_err(|e| KodError::MemoryDatabase(format!("Failed to remove: {}", e)))?;
        }
        
        txn.commit()
            .map_err(|e| KodError::MemoryDatabase(format!("Failed to commit: {}", e)))?;
        
        Ok(())
    }

    /// Get all entries
    pub async fn get_all(&self) -> Result<Vec<MemoryEntry>> {
        let txn = self.db.begin_read()
            .map_err(|e| KodError::MemoryDatabase(format!("Failed to start read transaction: {}", e)))?;
        
        let table = txn.open_table(MEMORY_TABLE)
            .map_err(|e| KodError::MemoryDatabase(format!("Failed to open table: {}", e)))?;
        
        let mut entries = Vec::new();
        
        for entry in table.iter() {
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
        
        Ok(all.into_iter()
            .filter(|e| e.content.to_lowercase().contains(&query_lower))
            .collect())
    }

    /// Count total entries
    pub async fn count(&self) -> Result<usize> {
        let txn = self.db.begin_read()
            .map_err(|e| KodError::MemoryDatabase(format!("Failed to start read transaction: {}", e)))?;
        
        let table = txn.open_table(MEMORY_TABLE)
            .map_err(|e| KodError::MemoryDatabase(format!("Failed to open table: {}", e)))?;
        
        let mut count = 0;
        for entry in table.iter() {
            if entry.is_ok() {
                count += 1;
            }
        }
        
        Ok(count)
    }

    /// Clear all entries (dangerous!)
    pub async fn clear(&self) -> Result<()> {
        let txn = self.db.begin_write()
            .map_err(|e| KodError::MemoryDatabase(format!("Failed to start transaction: {}", e)))?;
        
        {
            let mut table = txn.open_table(MEMORY_TABLE)
                .map_err(|e| KodError::MemoryDatabase(format!("Failed to open table: {}", e)))?;
            
            // Delete all entries
            while let Some(first) = table.iter().next() {
                if let Ok((key, _)) = first {
                    table.remove(key.key())
                        .map_err(|e| KodError::MemoryDatabase(format!("Failed to remove: {}", e)))?;
                } else {
                    break;
                }
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
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
cargo test -p kod-memory --test long_term
cargo test -p kod-memory --lib long_term
```

Expected: All tests pass

- [ ] **Step 5: Commit**

```bash
git add crates/kod-memory/
git commit -m "feat(memory): add long-term memory with redb persistent storage"
```

---

## Task 20: Episodic Memory (Vector-Based)

**Files:**
- Create: `crates/kod-memory/src/episodic.rs`
- Test: `crates/kod-memory/tests/episodic.rs`

- [ ] **Step 1: Write failing test for episodic memory**

Create `crates/kod-memory/tests/episodic.rs`:

```rust
use kod_memory::episodic::EpisodicMemory;
use kod_types::{EpisodicMemory as EpisodicMemoryType, MemoryId, Outcome};

fn create_episodic(content: &str, embedding: Vec<f32>) -> EpisodicMemoryType {
    EpisodicMemoryType {
        id: MemoryId::new(),
        content: content.to_string(),
        embedding,
        task_type: "coding".to_string(),
        outcome: Outcome::Success,
        timestamp: time::OffsetDateTime::now_utc(),
    }
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let dot_product: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let magnitude_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let magnitude_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    
    if magnitude_a == 0.0 || magnitude_b == 0.0 {
        0.0
    }  {
        dot_product / (magnitude_a * magnitude_b)
    }
}

#[tokio::test]
async fn test_store_and_retrieve() {
    let memory = EpisodicMemory::new();
    
    let episode = create_episodic(
        "Fixed a Rust lifetime error",
        vec![1.0, 0.0, 0.0],
    );
    
    memory.store(episode.clone()).await.unwrap();
    
    let retrieved = memory.get(&episode.id).await.unwrap();
    assert!(retrieved.is_some());
    assert_eq!(retrieved.unwrap().content, "Fixed a Rust lifetime error");
}

#[tokio::test]
async fn test_find_similar() {
    let memory = EpisodicMemory::new();
    
    // Store episodes with different embeddings
    memory.store(create_episodic(
        "Rust lifetime error",
        vec![1.0, 0.0, 0.0],
    )).await.unwrap();
    
    memory.store(create_episodic(
        "Python type error",
        vec![0.0, 1.0, 0.0],
    )).await.unwrap();
    
    memory.store(create_episodic(
        "Rust ownership error",
        vec![0.9, 0.1, 0.0], // Similar to first
    )).await.unwrap();
    
    // Find similar to Rust-like embedding
    let query_embedding = vec![1.0, 0.1, 0.0];
    let similar = memory.find_similar(&query_embedding, 2).await.unwrap();
    
    assert_eq!(similar.len(), 2);
    
    // First result should be most similar (Rust lifetime error or ownership error)
    let similarities: Vec<f32> = similar.iter()
        .map(|e| cosine_similarity(&query_embedding, &e.embedding))
        .collect();
    
    // Similarities should be in descending order
    assert!(similarities[0] >= similarities[1]);
}

#[tokio::test]
async test_semantic_search() {
    let memory = EpisodicMemory::new();
    
    // Store episodes about different topics
    memory.store(create_episodic(
        "Implemented user authentication in Rust",
        vec![0.8, 0.2, 0.0],
    )).await.unwrap();
    
    memory.store(create_episodic(
        "Fixed database connection leak",
        vec![0.1, 0.9, 0.0],
    )).await.unwrap();
    
    memory.store(create_episodic(
        "Added input validation to auth flow",
        vec![0.7, 0.3, 0.0], // Similar to auth-related
    )).await.unwrap();
    
    // Search for auth-related content
    let query_embedding = vec![0.75, 0.25, 0.0];
    let results = memory.semantic_search("authentication", &query_embedding, 0.5).await.unwrap();
    
    // Should find auth-related episodes
    assert!(!results.is_empty());
    assert!(results.iter().any(|e| e.content.contains("authentication")));
}

#[tokio::test]
async fn test_get_by_task_type() {
    let memory = EpisodicMemory::new();
    
    let mut coding_episode = create_episodic("Coding task", vec![1.0, 0.0, 0.0]);
    coding_episode.task_type = "coding".to_string();
    
    let mut debug_episode = create_episodic("Debugging task", vec![0.0, 1.0, 0.0]);
    debug_episode.task_type = "debugging".to_string();
    
    memory.store(coding_episode).await.unwrap();
    memory.store(debug_episode).await.unwrap();
    
    let coding_episodes = memory.get_by_task_type("coding").await.unwrap();
    assert_eq!(coding_episodes.len(), 1);
    assert_eq!(coding_episodes[0].task_type, "coding");
}

#[tokio::test]
async fn test_get_by_outcome() {
    let memory = EpisodicMemory::new();
    
    let mut success_episode = create_episodic("Successful task", vec![1.0, 0.0, 0.0]);
    success_episode.outcome = Outcome::Success;
    
    let mut failure_episode = create_episodic("Failed task", vec![0.0, 1.0, 0.0]);
    failure_episode.outcome = Outcome::Failure;
    
    memory.store(success_episode).await.unwrap();
    memory.store(failure_episode).await.unwrap();
    
    let successes = memory.get_by_outcome(Outcome::Success).await.unwrap();
    assert_eq!(successes.len(), 1);
    assert_eq!(successes[0].outcome, Outcome::Success);
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test -p kod-memory --test episodic
```

Expected: FAIL - episodic module not implemented

- [ ] **Step 3: Implement episodic memory**

Create `crates/kod-memory/src/episodic.rs`:

```rust
//! Episodic memory - vector-based storage for semantic search.
//!
//! Stores task experiences with embeddings for finding similar
//! past experiences. Initially in-memory, with optional persistence.

use kod_error::{KodError, Result};
use kod_types::{EpisodicMemory as EpisodicMemoryType, MemoryId, Outcome};
use std::collections::HashMap;
use parking_lot::RwLock;

/// In-memory episodic storage with vector similarity search
#[derive(Debug, Default)]
pub struct EpisodicMemory {
    episodes: RwLock<HashMap<MemoryId, EpisodicMemoryType>>,
}

impl EpisodicMemory {
    pub fn new() -> Self {
        Self {
            episodes: RwLock::new(HashMap::new()),
        }
    }

    /// Store an episode
    pub async fn store(&self, episode: EpisodicMemoryType) -> Result<()> {
        self.episodes.write().insert(episode.id.clone(), episode);
        Ok(())
    }

    /// Get an episode by ID
    pub async fn get(&self, id: &MemoryId) -> Result<Option<EpisodicMemoryType>> {
        Ok(self.episodes.read().get(id).cloned())
    }

    /// Remove an episode
    pub async fn remove(&self, id: &MemoryId) -> Result<Option<EpisodicMemoryType>> {
        Ok(self.episodes.write().remove(id))
    }

    /// Find similar episodes based on embedding similarity
    pub async fn find_similar(
        &self,
        query_embedding: &[f32],
        limit: usize,
    ) -> Result<Vec<EpisodicMemoryType>> {
        let episodes = self.episodes.read();
        
        let mut scored: Vec<(f32, &EpisodicMemoryType)> = episodes.values()
            .map(|episode| {
                let similarity = cosine_similarity(query_embedding, &episode.embedding);
                (similarity, episode)
            })
            .collect();
        
        // Sort by similarity (descending)
        scored.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        
        Ok(scored.into_iter()
            .take(limit)
            .map(|(_, episode)| episode.clone())
            .collect())
    }

    /// Semantic search combining text query and embedding
    pub async fn semantic_search(
        &self,
        text_query: &str,
        query_embedding: &[f32],
        min_similarity: f32,
    ) -> Result<Vec<EpisodicMemoryType>> {
        let text_lower = text_query.to_lowercase();
        let episodes = self.episodes.read();
        
        let mut results: Vec<EpisodicMemoryType> = Vec::new();
        
        for episode in episodes.values() {
            // Calculate combined score
            let text_match = episode.content.to_lowercase().contains(&text_lower);
            let embedding_similarity = cosine_similarity(query_embedding, &episode.embedding);
            
            // Include if either text matches or embedding is similar enough
            if text_match || embedding_similarity >= min_similarity {
                results.push(episode.clone());
            }
        }
        
        // Sort by relevance (embedding similarity as proxy)
        results.sort_by(|a, b| {
            let sim_a = cosine_similarity(query_embedding, &a.embedding);
            let sim_b = cosine_similarity(query_embedding, &b.embedding);
            sim_b.partial_cmp(&sim_a)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        
        Ok(results)
    }

    /// Get episodes by task type
    pub async fn get_by_task_type(&self, task_type: &str) -> Result<Vec<EpisodicMemoryType>> {
        let episodes = self.episodes.read();
        
        Ok(episodes.values()
            .filter(|e| e.task_type == task_type)
            .cloned()
            .collect())
    }

    /// Get episodes by outcome
    pub async fn get_by_outcome(&self, outcome: Outcome) -> Result<Vec<EpisodicMemoryType>> {
        let episodes = self.episodes.read();
        
        Ok(episodes.values()
            .filter(|e| e.outcome == outcome)
            .cloned()
            .collect())
    }

    /// Get all episodes
    pub async fn get_all(&self) -> Result<Vec<EpisodicMemoryType>> {
        Ok(self.episodes.read().values().cloned().collect())
    }

    /// Count total episodes
    pub async fn count(&self) -> Result<usize> {
        Ok(self.episodes.read().len())
    }

    /// Clear all episodes
    pub async fn clear(&self) -> Result<()> {
        self.episodes.write().clear();
        Ok(())
    }
}

/// Calculate cosine similarity between two vectors
fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    
    let dot_product: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let magnitude_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let magnitude_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    
    if magnitude_a == 0.0 || magnitude_b == 0.0 {
        0.0
    } else {
        dot_product / (magnitude_a * magnitude_b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cosine_similarity() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![1.0, 0.0, 0.0];
        assert!((cosine_similarity(&a, &b) - 1.0).abs() < 1e-6);
        
        let c = vec![0.0, 1.0, 0.0];
        assert!((cosine_similarity(&a, &c) - 0.0).abs() < 1e-6);
        
        let d = vec![1.0, 1.0, 0.0];
        assert!((cosine_similarity(&a, &d) - 0.7071).abs() < 1e-3);
    }

    #[test]
    fn test_zero_vectors() {
        let a = vec![0.0, 0.0, 0.0];
        let b = vec![1.0, 0.0, 0.0];
        assert_eq!(cosine_similarity(&a, &b), 0.0);
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
cargo test -p kod-memory --test episodic
cargo test -p kod-memory --lib episodic
```

Expected: All tests pass

- [ ] **Step 5: Commit**

```bash
git add crates/kod-memory/
git commit -m "feat(memory): add episodic memory with vector similarity search"
```

---

## Task 21: Memory Manager (Unified Interface)

**Files:**
- Create: `crates/kod-memory/src/manager.rs`
- Test: `crates/kod-memory/tests/manager.rs`

- [ ] **Step 1: Write failing test for memory manager**

Create `crates/kod-memory/tests/manager.rs`:

```rust
use kod_memory::manager::MemoryManager;
use kod_types::{MemoryEntry, MemoryType};
use tempfile::TempDir;

fn create_test_manager() -> (MemoryManager, TempDir) {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("memory.redb");
    
    let manager = MemoryManager::new(db_path, 100).unwrap();
    (manager, temp_dir)
}

#[tokio::test]
async fn test_store_short_term() {
    let (mut manager, _temp) = create_test_manager();
    
    let id = manager.store(
        MemoryType::ShortTerm,
        "Current session context",
    ).await.unwrap();
    
    let retrieved = manager.get_short_term(&id);
    assert!(retrieved.is_some());
    assert_eq!(retrieved.unwrap().content, "Current session context");
}

#[tokio::test]
async fn test_store_long_term() {
    let (mut manager, _temp) = create_test_manager();
    
    let id = manager.store(
        MemoryType::LongTerm,
        "User prefers dark mode",
    ).await.unwrap();
    
    let retrieved = manager.get_long_term(&id).await.unwrap();
    assert!(retrieved.is_some());
    assert_eq!(retrieved.unwrap().content, "User prefers dark mode");
}

#[tokio::test]
async fn test_store_episodic() {
    let (mut manager, _temp) = create_test_manager();
    
    let id = manager.store(
        MemoryType::Episodic,
        "Fixed a Rust lifetime error",
    ).await.unwrap();
    
    // Episodic memory should be retrievable
    let all = manager.get_all_episodic().await.unwrap();
    assert!(!all.is_empty());
}

#[tokio::test]
async fn test_retrieve_context() {
    let (mut manager, _temp) = create_test_manager();
    
    // Store different types of memories
    manager.store(MemoryType::ShortTerm, "Working on auth system").await.unwrap();
    manager.store(MemoryType::LongTerm, "User prefers Rust").await.unwrap();
    manager.store(MemoryType::Episodic, "Implemented auth before").await.unwrap();
    
    // Retrieve context for a query
    let context = manager.retrieve_context("auth implementation").await.unwrap();
    
    // Context should contain relevant information
    assert!(!context.working_memory.is_empty() || !context.long_term.is_empty());
}

#[tokio::test]
async fn test_short_term_capacity() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("memory.redb");
    
    let mut manager = MemoryManager::new(db_path, 3).unwrap();
    
    // Store more than capacity
    for i in 0..5 {
        manager.store(MemoryType::ShortTerm, &format!("Entry {}", i)).await.unwrap();
    }
    
    // Should only have 3 entries
    let all = manager.get_all_short_term();
    assert_eq!(all.len(), 3);
}

#[tokio::test]
async fn test_search_across_types() {
    let (mut manager, _temp) = create_test_manager();
    
    manager.store(MemoryType::ShortTerm, "Rust programming session").await.unwrap();
    manager.store(MemoryType::LongTerm, "User knows Python").await.unwrap();
    
    // Search should find relevant entries
    let results = manager.search("Rust").await.unwrap();
    assert!(!results.is_empty());
    
    let results = manager.search("Python").await.unwrap();
    assert!(!results.is_empty());
}

#[tokio::test]
async fn test_update_memory() {
    let (mut manager, _temp) = create_test_manager();
    
    let id = manager.store(MemoryType::ShortTerm, "Original content").await.unwrap();
    
    // Update the memory
    manager.update(
        MemoryType::ShortTerm,
        &id,
        "Updated content",
    ).await.unwrap();
    
    let retrieved = manager.get_short_term(&id);
    assert!(retrieved.is_some());
    assert_eq!(retrieved.unwrap().content, "Updated content");
}

#[tokio::test]
async fn test_remove_memory() {
    let (mut manager, _temp) = create_test_manager();
    
    let id = manager.store(MemoryType::ShortTerm, "To be removed").await.unwrap();
    
    manager.remove(MemoryType::ShortTerm, &id).await.unwrap();
    
    let retrieved = manager.get_short_term(&id);
    assert!(retrieved.is_none());
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test -p kod-memory --test manager
```

Expected: FAIL - manager module not implemented

- [ ] **Step 3: Implement memory manager**

Create `crates/kod-memory/src/manager.rs`:

```rust
//! Memory manager - unified interface for all memory types.
//!
//! Coordinates short-term, long-term, and episodic memory
//! to provide a single API for storing and retrieving context.

use crate::{episodic::EpisodicMemory, long_term::LongTermMemory, short_term::ShortTermMemory};
use kod_error::{KodError, Result};
use kod_types::{
    EpisodicMemory as EpisodicMemoryType, MemoryContext, MemoryEntry, MemoryId, MemoryType,
    Outcome,
};
use std::path::PathBuf;
use time::OffsetDateTime;

/// Unified memory manager
pub struct MemoryManager {
    short_term: ShortTermMemory,
    long_term: LongTermMemory,
    episodic: EpisodicMemory,
    context_window: usize,
}

impl MemoryManager {
    /// Create a new memory manager
    pub fn new(db_path: PathBuf, short_term_capacity: usize) -> Result<Self> {
        let long_term = LongTermMemory::new(&db_path)?;
        
        Ok(Self {
            short_term: ShortTermMemory::new(short_term_capacity),
            long_term,
            episodic: EpisodicMemory::new(),
            context_window: 4096, // Default context window
        })
    }

    /// Set the context window size (in tokens)
    pub fn set_context_window(&mut self, tokens: usize) {
        self.context_window = tokens;
    }

    /// Store content in the specified memory type
    pub async fn store(&self, memory_type: MemoryType, content: &str) -> Result<MemoryId> {
        let id = MemoryId::new();
        
        match memory_type {
            MemoryType::ShortTerm => {
                let entry = MemoryEntry {
                    id: id.clone(),
                    memory_type,
                    content: content.to_string(),
                    timestamp: OffsetDateTime::now_utc(),
                    relevance: 1.0,
                    metadata: Default::default(),
                };
                self.short_term.store(entry);
            }
            MemoryType::LongTerm => {
                let entry = MemoryEntry {
                    id: id.clone(),
                    memory_type,
                    content: content.to_string(),
                    timestamp: OffsetDateTime::now_utc(),
                    relevance: 0.8,
                    metadata: Default::default(),
                };
                self.long_term.store(entry).await?;
            }
            MemoryType::Episodic => {
                // For now, use a simple embedding (in production, would use fastembed)
                let embedding = self.generate_simple_embedding(content);
                
                let episode = EpisodicMemoryType {
                    id: id.clone(),
                    content: content.to_string(),
                    embedding,
                    task_type: "general".to_string(),
                    outcome: Outcome::Success,
                    timestamp: OffsetDateTime::now_utc(),
                };
                self.episodic.store(episode).await?;
            }
            MemoryType::Semantic => {
                // Semantic memory would be stored differently (graph DB)
                // For now, treat as long-term
                let entry = MemoryEntry {
                    id: id.clone(),
                    memory_type,
                    content: content.to_string(),
                    timestamp: OffsetDateTime::now_utc(),
                    relevance: 0.9,
                    metadata: Default::default(),
                };
                self.long_term.store(entry).await?;
            }
        }
        
        Ok(id)
    }

    /// Get a short-term memory entry
    pub fn get_short_term(&self, id: &MemoryId) -> Option<MemoryEntry> {
        self.short_term.get(id)
    }

    /// Get a long-term memory entry
    pub async fn get_long_term(&self, id: &MemoryId) -> Result<Option<MemoryEntry>> {
        self.long_term.get(id).await
    }

    /// Update memory content
    pub async fn update(&self, memory_type: MemoryType, id: &MemoryId, content: &str) -> Result<()> {
        match memory_type {
            MemoryType::ShortTerm => {
                // Short-term memory doesn't support update, so remove and re-add
                if let Some(mut entry) = self.short_term.get(id) {
                    entry.content = content.to_string();
                    self.short_term.store(entry);
                    Ok(())
                } else {
                    Err(KodError::MemoryStorage("Entry not found".to_string()))
                }
            }
            MemoryType::LongTerm | MemoryType::Semantic => {
                if let Some(mut entry) = self.long_term.get(id).await? {
                    entry.content = content.to_string();
                    self.long_term.store(entry).await?;
                    Ok(())
                } else {
                    Err(KodError::MemoryStorage("Entry not found".to_string()))
                }
            }
            MemoryType::Episodic => {
                // Episodic memory update would need embedding regeneration
                // For now, return error
                Err(KodError::MemoryStorage("Episodic memory update not supported".to_string()))
            }
        }
    }

    /// Remove memory entry
    pub async fn remove(&self, memory_type: MemoryType, id: &MemoryId) -> Result<()> {
        match memory_type {
            MemoryType::ShortTerm => {
                self.short_term.remove(id);
                Ok(())
            }
            MemoryType::LongTerm | MemoryType::Semantic => {
                self.long_term.remove(id).await
            }
            MemoryType::Episodic => {
                self.episodic.remove(id).await?;
                Ok(())
            }
        }
    }

    /// Search across all memory types
    pub async fn search(&self, query: &str) -> Result<Vec<MemoryEntry>> {
        let mut results = Vec::new();
        
        // Search short-term
        results.extend(self.short_term.search(query));
        
        // Search long-term
        results.extend(self.long_term.search(query).await?);
        
        // Sort by relevance
        results.sort_by(|a, b| {
            b.relevance.partial_cmp(&a.relevance)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        
        Ok(results)
    }

    /// Retrieve context for a query
    pub async fn retrieve_context(&self, query: &str) -> Result<MemoryContext> {
        let mut context = MemoryContext::default();
        
        // 1. Get working memory (recent short-term)
        context.working_memory = self.short_term.get_recent(10);
        
        // 2. Get relevant long-term memories
        context.long_term = self.long_term.search(query).await?;
        
        // 3. Get similar episodic memories
        // For now, use text search in episodic memory
        let all_episodic = self.episodic.get_all().await?;
        let query_lower = query.to_lowercase();
        context.episodic = all_episodic.into_iter()
            .filter(|e| e.content.to_lowercase().contains(&query_lower))
            .collect();
        
        // 4. Limit total context size
        self.limit_context_size(&mut context);
        
        Ok(context)
    }

    /// Get all short-term memories
    pub fn get_all_short_term(&self) -> Vec<MemoryEntry> {
        self.short_term.get_all()
    }

    /// Get all long-term memories
    pub async fn get_all_long_term(&self) -> Result<Vec<MemoryEntry>> {
        self.long_term.get_all().await
    }

    /// Get all episodic memories
    pub async fn get_all_episodic(&self) -> Result<Vec<EpisodicMemoryType>> {
        self.episodic.get_all().await
    }

    /// Clear all memories
    pub async fn clear_all(&self) -> Result<()> {
        self.short_term.clear();
        self.long_term.clear().await?;
        self.episodic.clear().await?;
        Ok(())
    }

    /// Clear short-term memory only
    pub fn clear_short_term(&self) {
        self.short_term.clear();
    }

    /// Generate a simple embedding (placeholder for fastembed)
    fn generate_simple_embedding(&self, content: &str) -> Vec<f32> {
        // Simple hash-based embedding for testing
        // In production, this would use fastembed
        let mut embedding = vec![0.0; 128];
        
        for (i, byte) in content.bytes().enumerate() {
            let index = (byte as usize) % 128;
            embedding[index] += 1.0;
        }
        
        // Normalize
        let magnitude: f32 = embedding.iter().map(|x| x * x).sum::<f32>().sqrt();
        if magnitude > 0.0 {
            for value in embedding.iter_mut() {
                *value /= magnitude;
            }
        }
        
        embedding
    }

    /// Limit context size to fit within context window
    fn limit_context_size(&self, context: &mut MemoryContext) {
        // Rough estimation: 1 token ≈ 4 characters
        let max_chars = self.context_window * 4;
        let mut total_chars = 0;
        
        // Limit working memory
        let mut working = Vec::new();
        for entry in context.working_memory.drain(..) {
            total_chars += entry.content.len();
            if total_chars <= max_chars {
                working.push(entry);
            } else {
                break;
            }
        }
        context.working_memory = working;
        
        // Limit long-term
        let mut long_term = Vec::new();
        for entry in context.long_term.drain(..) {
            total_chars += entry.content.len();
            if total_chars <= max_chars {
                long_term.push(entry);
            } else {
                break;
            }
        }
        context.long_term = long_term;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_manager_basic_operations() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");
        
        let manager = MemoryManager::new(db_path, 10).unwrap();
        
        // Store in different types
        let short_id = manager.store(MemoryType::ShortTerm, "Short term").await.unwrap();
        let long_id = manager.store(MemoryType::LongTerm, "Long term").await.unwrap();
        
        // Retrieve
        assert!(manager.get_short_term(&short_id).is_some());
        assert!(manager.get_long_term(&long_id).await.unwrap().is_some());
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
cargo test -p kod-memory --test manager
cargo test -p kod-memory --lib manager
```

Expected: All tests pass

- [ ] **Step 5: Commit**

```bash
git add crates/kod-memory/
git commit -m "feat(memory): add unified memory manager coordinating all memory types"
```

---

## Task 22: Context Builder

**Files:**
- Create: `crates/kod-memory/src/context.rs`
- Test: `crates/kod-memory/tests/context.rs`

- [ ] **Step 1: Write failing test for context builder**

Create `crates/kod-memory/tests/context.rs`:

```rust
use kod_memory::context::ContextBuilder;
use kod_types::{
    MemoryContext, MemoryEntry, MemoryType, Skill,
};
use time::OffsetDateTime;

fn create_memory_entry(content: &str, relevance: f32) -> MemoryEntry {
    MemoryEntry {
        id: kod_types::MemoryId::new(),
        memory_type: MemoryType::LongTerm,
        content: content.to_string(),
        timestamp: OffsetDateTime::now_utc(),
        relevance,
        metadata: Default::default(),
    }
}

#[test]
fn test_build_basic_context() {
    let builder = ContextBuilder::new();
    
    let memory_context = MemoryContext {
        working_memory: vec![create_memory_entry("Current task", 1.0)],
        long_term: vec![create_memory_entry("User preference", 0.8)],
        episodic: Vec::new(),
        total_tokens: 100,
    };
    
    let context = builder
        .with_user_input("Help me with Rust")
        .with_memory(memory_context)
        .build();
    
    assert_eq!(context.user_input, "Help me with Rust");
    assert_eq!(context.memory_context.working_memory.len(), 1);
    assert_eq!(context.memory_context.long_term.len(), 1);
}

#[test]
fn test_add_skills() {
    let builder = ContextBuilder::new();
    
    let skill = Skill {
        id: kod_types::SkillId::new(),
        metadata: kod_types::SkillMetadata {
            name: "rust-help".to_string(),
            description: "Rust help".to_string(),
            version: "1.0.0".to_string(),
            author: None,
            category: "coding".to_string(),
            tags: vec!["rust".to_string()],
            capabilities: vec!["help".to_string()],
            requirements: Vec::new(),
            triggers: Vec::new(),
        },
        instructions: "Rust instructions".to_string(),
        examples: Vec::new(),
        constraints: None,
        content: String::new(),
        path: std::path::PathBuf::new(),
    };
    
    let context = builder
        .with_user_input("Rust question")
        .with_skills(vec![skill])
        .build();
    
    assert_eq!(context.skills.len(), 1);
    assert_eq!(context.skills[0].metadata.name, "rust-help");
}

#[test]
fn test_context_serialization() {
    let builder = ContextBuilder::new();
    
    let context = builder
        .with_user_input("Test input")
        .build();
    
    // Context should be serializable
    let json = serde_json::to_string(&context).unwrap();
    assert!(json.contains("Test input"));
}

#[test]
fn test_context_token_estimation() {
    let builder = ContextBuilder::new();
    
    let memory_context = MemoryContext {
        working_memory: vec![create_memory_entry(&"x".repeat(100), 1.0)],
        long_term: Vec::new(),
        episodic: Vec::new(),
        total_tokens: 0,
    };
    
    let context = builder
        .with_user_input("Test")
        .with_memory(memory_context)
        .build();
    
    // Should estimate tokens (rough: 4 chars per token)
    assert!(context.estimated_tokens() > 0);
}

#[test]
fn test_context_to_prompt() {
    let builder = ContextBuilder::new();
    
    let memory_context = MemoryContext {
        working_memory: vec![create_memory_entry("User is working on auth", 1.0)],
        long_term: vec![create_memory_entry("User prefers Rust", 0.8)],
        episodic: Vec::new(),
        total_tokens: 0,
    };
    
    let context = builder
        .with_user_input("How do I implement auth?")
        .with_memory(memory_context)
        .build();
    
    let prompt = context.to_prompt();
    
    // Prompt should contain context information
    assert!(prompt.contains("How do I implement auth?"));
    assert!(prompt.contains("User is working on auth"));
    assert!(prompt.contains("User prefers Rust"));
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test -p kod-memory --test context
```

Expected: FAIL - context module not implemented

- [ ] **Step 3: Implement context builder**

Create `crates/kod-memory/src/context.rs`:

```rust
//! Context builder - assembles context from various sources for LLM prompts.
//!
//! Combines user input, memory, and skills into a structured context
//! that can be serialized into a prompt.

use kod_types::{MemoryContext, Skill};
use serde::{Deserialize, Serialize};

/// Built context ready for LLM processing
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Context {
    pub user_input: String,
    pub memory_context: MemoryContext,
    pub skills: Vec<Skill>,
    pub system_prompt: Option<String>,
}

impl Context {
    /// Estimate token count (rough approximation)
    pub fn estimated_tokens(&self) -> usize {
        let total_chars = self.user_input.len()
            + self.memory_context.working_memory.iter()
                .map(|m| m.content.len()).sum::<usize>()
            + self.memory_context.long_term.iter()
                .map(|m| m.content.len()).sum::<usize>()
            + self.skills.iter()
                .map(|s| s.content.len()).sum::<usize>();
        
        // Rough: 1 token ≈ 4 characters
        total_chars / 4
    }

    /// Convert to a prompt string for the LLM
    pub fn to_prompt(&self) -> String {
        let mut prompt = String::new();
        
        // Add system context from memory
        if !self.memory_context.working_memory.is_empty() {
            prompt.push_str("## Current Context\n\n");
            for entry in &self.memory_context.working_memory {
                prompt.push_str(&format!("- {}\n", entry.content));
            }
            prompt.push('\n');
        }
        
        if !self.memory_context.long_term.is_empty() {
            prompt.push_str("## User Preferences & Knowledge\n\n");
            for entry in &self.memory_context.long_term {
                prompt.push_str(&format!("- {}\n", entry.content));
            }
            prompt.push('\n');
        }
        
        // Add skill instructions
        if !self.skills.is_empty() {
            prompt.push_str("## Relevant Skills\n\n");
            for skill in &self.skills {
                prompt.push_str(&format!("### {}\n\n{}\n\n", 
                    skill.metadata.name, 
                    skill.instructions
                ));
            }
        }
        
        // Add system prompt if provided
        if let Some(system) = &self.system_prompt {
            prompt.push_str(&format!("## System\n\n{}\n\n", system));
        }
        
        // Add user input
        prompt.push_str(&format!("## User Request\n\n{}", self.user_input));
        
        prompt
    }
}

/// Builder for Context
#[derive(Debug, Default)]
pub struct ContextBuilder {
    user_input: String,
    memory_context: MemoryContext,
    skills: Vec<Skill>,
    system_prompt: Option<String>,
}

impl ContextBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the user input
    pub fn with_user_input(mut self, input: impl Into<String>) -> Self {
        self.user_input = input.into();
        self
    }

    /// Set the memory context
    pub fn with_memory(mut self, memory: MemoryContext) -> Self {
        self.memory_context = memory;
        self
    }

    /// Add skills to the context
    pub fn with_skills(mut self, skills: Vec<Skill>) -> Self {
        self.skills = skills;
        self
    }

    /// Add a system prompt
    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = Some(prompt.into());
        self
    }

    /// Build the final context
    pub fn build(self) -> Context {
        Context {
            user_input: self.user_input,
            memory_context: self.memory_context,
            skills: self.skills,
            system_prompt: self.system_prompt,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_context_builder() {
        let context = ContextBuilder::new()
            .with_user_input("Test input")
            .build();
        
        assert_eq!(context.user_input, "Test input");
    }

    #[test]
    fn test_prompt_generation() {
        let context = ContextBuilder::new()
            .with_user_input("Help me")
            .build();
        
        let prompt = context.to_prompt();
        assert!(prompt.contains("Help me"));
        assert!(prompt.contains("## User Request"));
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
cargo test -p kod-memory --test context
cargo test -p kod-memory --lib context
```

Expected: All tests pass

- [ ] **Step 5: Commit**

```bash
git add crates/kod-memory/
git commit -m "feat(memory): add context builder for assembling LLM prompts"
```

---

## Task 23: Memory Integration Test

**Files:**
- Create: `crates/kod-memory/tests/integration.rs`

- [ ] **Step 1: Write integration test for full memory pipeline**

Create `crates/kod-memory/tests/integration.rs`:

```rust
use kod_memory::{context::ContextBuilder, manager::MemoryManager};
use kod_types::MemoryType;
use tempfile::TempDir;

#[tokio::test]
async fn test_full_memory_pipeline() {
    // 1. Set up memory manager
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("memory.redb");
    
    let manager = MemoryManager::new(db_path, 100).unwrap();
    
    // 2. Store different types of memories
    manager.store(MemoryType::ShortTerm, "Currently working on authentication").await.unwrap();
    manager.store(MemoryType::LongTerm, "User prefers Rust over Python").await.unwrap();
    manager.store(MemoryType::LongTerm, "Project uses Tokio runtime").await.unwrap();
    manager.store(MemoryType::Episodic, "Implemented JWT auth successfully").await.unwrap();
    
    // 3. Retrieve context for a query
    let memory_context = manager.retrieve_context("authentication implementation").await.unwrap();
    
    // Should have relevant memories
    assert!(!memory_context.working_memory.is_empty());
    
    // 4. Build context
    let context = ContextBuilder::new()
        .with_user_input("How should I implement authentication?")
        .with_memory(memory_context)
        .build();
    
    // 5. Generate prompt
    let prompt = context.to_prompt();
    
    // Prompt should contain relevant context
    assert!(prompt.contains("authentication"));
    assert!(prompt.contains("How should I implement authentication?"));
}

#[tokio::test]
async fn test_memory_persistence() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("memory.redb");
    
    // Store memories in first instance
    {
        let manager = MemoryManager::new(db_path.clone(), 100).unwrap();
        
        manager.store(MemoryType::LongTerm, "Persistent fact 1").await.unwrap();
        manager.store(MemoryType::LongTerm, "Persistent fact 2").await.unwrap();
    }
    
    // Create new instance and verify persistence
    {
        let manager = MemoryManager::new(db_path, 100).unwrap();
        
        let all_long_term = manager.get_all_long_term().await.unwrap();
        assert_eq!(all_long_term.len(), 2);
        
        let contents: Vec<String> = all_long_term.iter()
            .map(|m| m.content.clone())
            .collect();
        
        assert!(contents.contains(&"Persistent fact 1".to_string()));
        assert!(contents.contains(&"Persistent fact 2".to_string()));
    }
}

#[tokio::test]
async fn test_memory_search_across_types() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("memory.redb");
    
    let manager = MemoryManager::new(db_path, 100).unwrap();
    
    // Store memories in different types
    manager.store(MemoryType::ShortTerm, "Working with Rust async").await.unwrap();
    manager.store(MemoryType::LongTerm, "User likes functional programming").await.unwrap();
    manager.store(MemoryType::Episodic, "Solved async Rust bug").await.unwrap();
    
    // Search for "Rust"
    let results = manager.search("Rust").await.unwrap();
    assert!(!results.is_empty());
    
    // Search for "functional"
    let results = manager.search("functional").await.unwrap();
    assert!(!results.is_empty());
}

#[tokio::test]
async fn test_memory_cleanup() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("memory.redb");
    
    let manager = MemoryManager::new(db_path, 100).unwrap();
    
    // Store some memories
    manager.store(MemoryType::ShortTerm, "Temp memory").await.unwrap();
    manager.store(MemoryType::LongTerm, "Persistent memory").await.unwrap();
    
    // Clear short-term only
    manager.clear_short_term();
    
    let short_term = manager.get_all_short_term();
    assert!(short_term.is_empty());
    
    let long_term = manager.get_all_long_term().await.unwrap();
    assert!(!long_term.is_empty());
}
```

- [ ] **Step 2: Run all memory tests**

```bash
cargo test -p kod-memory
```

Expected: All tests pass

- [ ] **Step 3: Verify workspace builds**

```bash
cargo build --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

Expected: Build succeeds with no warnings

- [ ] **Step 4: Commit**

```bash
git add crates/kod-memory/
git commit -m "feat(memory): add integration tests for full memory pipeline"
```

---

## Chunk 4 Review Checklist

- [ ] Short-term memory stores and evicts entries correctly (FIFO)
- [ ] Long-term memory persists across process restarts (redb)
- [ ] Episodic memory supports vector similarity search
- [ ] Memory manager provides unified interface for all types
- [ ] Context builder assembles prompt from memory and skills
- [ ] Context can be serialized and converted to prompt string
- [ ] Integration tests verify the full pipeline works
- [ ] All tests pass
- [ ] Clippy passes with no warnings

**Verification commands:**

```bash
cargo test -p kod-memory
cargo clippy -p kod-memory -- -D warnings
cargo build --workspace
```

---

## Chunk 4 Summary

**Implemented:**
1. **Short-Term Memory** (`short_term.rs`)
   - In-memory storage with FIFO eviction
   - Capacity limits
   - Search and retrieval by ID
   - Thread-safe with parking_lot RwLock

2. **Long-Term Memory** (`long_term.rs`)
   - Persistent storage using redb
   - ACID transactions
   - Search by content
   - survives process restarts

3. **Episodic Memory** (`episodic.rs`)
   - Vector-based similarity search
   - Cosine similarity for embeddings
   - Task type and outcome filtering
   - Semantic search combining text and embeddings

4. **Memory Manager** (`manager.rs`)
   - Unified interface for all memory types
   - Context retrieval with size limiting
   - Search across all memory types
   - Simple embedding generation (placeholder for fastembed)

5. **Context Builder** (`context.rs`)
   - Assembles context from user input, memory, and skills
   - Token estimation
   - Prompt generation for LLM
   - Serializable context

**Next Chunk Preview:**

Chunk 5 will cover the **Tool System** implementation:
- Tool registry and trait definitions
- File system tools (read, write, list)
- Git tools (status, diff)
- Tool executor with sandboxing
- Tool result handling

Would you like me to continue with **Chunk 5: Tool System**?
