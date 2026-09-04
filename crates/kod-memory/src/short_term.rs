//! Short-term memory - in-memory storage with capacity limits.
//!
//! Uses FIFO eviction when capacity is exceeded. Suitable for
//! session-scoped context that doesn't need persistence.

use kod_types::{MemoryEntry, MemoryId};
use parking_lot::RwLock;
use std::collections::HashMap;

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
            entries.iter().cloned().collect()
        } else {
            entries[entries.len() - count..].to_vec()
        }
    }

    /// Get all entries
    pub fn get_all(&self) -> Vec<MemoryEntry> {
        self.entries.read().iter().cloned().collect()
    }

    /// Search entries by content (case-insensitive substring match)
    pub fn search(&self, query: &str) -> Vec<MemoryEntry> {
        let query_lower = query.to_lowercase();
        let entries = self.entries.read();

        entries
            .iter()
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
