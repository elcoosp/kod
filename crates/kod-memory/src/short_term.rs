//! Short-term memory - in-memory storage with capacity limits.
//!
//! Uses FIFO eviction when capacity is exceeded. Suitable for
//! session-scoped context that doesn't need persistence.

use kod_types::{MemoryEntry, MemoryId};
use parking_lot::RwLock;
use std::collections::HashMap;

/// H-D6: single lock guarding entries + index. The pre-fix shape
/// had two `RwLock`s and `store` took `entries→index` while `remove`
/// took `index→entries` — a classic lock-order inversion. Today
/// `remove` is unreachable in production, but the deadlock is latent
/// and the shape bought nothing (both maps are always updated
/// together).
#[derive(Debug, Default)]
struct ShortTermState {
    entries: Vec<MemoryEntry>,
    index: HashMap<MemoryId, usize>,
}

/// In-memory short-term storage with FIFO eviction
#[derive(Debug)]
pub struct ShortTermMemory {
    state: RwLock<ShortTermState>,
    capacity: usize,
}

impl ShortTermMemory {
    /// Create a new short-term memory with the given capacity
    pub fn new(capacity: usize) -> Self {
        Self {
            state: RwLock::new(ShortTermState {
                entries: Vec::with_capacity(capacity),
                index: HashMap::with_capacity(capacity),
            }),
            capacity,
        }
    }

    /// Store an entry. H-D6: an entry whose id is already present is
    /// *replaced in place* rather than appended. The pre-fix `store`
    /// pushed a second copy unconditionally, corrupting the index
    /// (which kept only one position) and leaving a stale twin that
    /// `remove` could not reach.
    pub fn store(&self, entry: MemoryEntry) {
        let mut st = self.state.write();

        if let Some(&pos) = st.index.get(&entry.id) {
            st.entries[pos] = entry;
            return;
        }

        if st.entries.len() >= self.capacity {
            // Evict oldest.
            let ShortTermState { entries, index } = &mut *st;
            if let Some(oldest) = entries.first() {
                let old_id = oldest.id.clone();
                entries.remove(0);
                index.remove(&old_id);
                for e in entries.iter() {
                    if let Some(p) = index.get_mut(&e.id) {
                        *p = p.saturating_sub(1);
                    }
                }
            }
        }

        let position = st.entries.len();
        st.index.insert(entry.id.clone(), position);
        st.entries.push(entry);
    }

    /// Get an entry by ID
    pub fn get(&self, id: &MemoryId) -> Option<MemoryEntry> {
        let st = self.state.read();
        let &position = st.index.get(id)?;
        st.entries.get(position).cloned()
    }

    /// Remove an entry by ID
    pub fn remove(&self, id: &MemoryId) -> Option<MemoryEntry> {
        let mut st = self.state.write();
        let ShortTermState { entries, index } = &mut *st;
        let position = index.remove(id)?;
        if position >= entries.len() {
            return None;
        }
        let removed = entries.remove(position);
        for entry in entries.iter().skip(position) {
            if let Some(pos) = index.get_mut(&entry.id) {
                *pos = pos.saturating_sub(1);
            }
        }
        Some(removed)
    }

    /// Keep only the most recent `target` entries, dropping older
    /// ones. Returns the number of entries removed.
    ///
    /// The store path already evicts the *oldest* entry one at a time
    /// when capacity is exceeded — enough for correctness, thrashy at
    /// the boundary. This method lets a caller trim further ahead of a
    /// burst so the working set sits below the cap and `store`'s own
    /// eviction does not fire on the next write.
    ///
    /// `target` is clamped to `capacity`: retaining more than the
    /// in-memory limit would fight `store`'s eviction, which is
    /// hard-capped at `capacity`.
    pub fn retain_recent(&self, target: usize) -> usize {
        let target = target.min(self.capacity);
        let mut st = self.state.write();
        let ShortTermState { entries, index } = &mut *st;
        if entries.len() <= target {
            return 0;
        }
        let drop = entries.len() - target;
        entries.drain(..drop);
        index.clear();
        for (i, entry) in entries.iter().enumerate() {
            index.insert(entry.id.clone(), i);
        }
        drop
    }

    /// Get the N most recent entries
    pub fn get_recent(&self, count: usize) -> Vec<MemoryEntry> {
        let st = self.state.read();
        if st.entries.len() <= count {
            st.entries.clone()
        } else {
            st.entries[st.entries.len() - count..].to_vec()
        }
    }

    /// Get all entries
    pub fn get_all(&self) -> Vec<MemoryEntry> {
        self.state.read().entries.clone()
    }

    /// Search entries by content (case-insensitive substring match)
    pub fn search(&self, query: &str) -> Vec<MemoryEntry> {
        let query_lower = query.to_lowercase();
        self.state
            .read()
            .entries
            .iter()
            .filter(|e| e.content.to_lowercase().contains(&query_lower))
            .cloned()
            .collect()
    }

    /// Clear all entries
    pub fn clear(&self) {
        let mut st = self.state.write();
        st.entries.clear();
        st.index.clear();
    }

    /// Get current number of entries
    pub fn len(&self) -> usize {
        self.state.read().entries.len()
    }

    /// Check if memory is empty
    pub fn is_empty(&self) -> bool {
        self.state.read().entries.is_empty()
    }

    /// Get capacity
    pub fn capacity(&self) -> usize {
        self.capacity
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
        
            superseded_by: None,
            contradicts: Vec::new(),
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
    fn test_retain_recent_keeps_newest() {
        let memory = ShortTermMemory::new(10);
        for i in 0..10 {
            memory.store(create_entry(&format!("entry-{i}")));
        }
        let removed = memory.retain_recent(4);
        assert_eq!(removed, 6);
        assert_eq!(memory.len(), 4);

        // The retained entries are the *newest*: entry-6 through
        // entry-9, in order.
        let all = memory.get_all();
        assert_eq!(all[0].content, "entry-6");
        assert_eq!(all[3].content, "entry-9");

        // Index is consistent — get() finds the retained entries by
        // id after the rebuild.
        let id0 = all[0].id.clone();
        let got = memory.get(&id0).expect("retained entry must be gettable");
        assert_eq!(got.content, "entry-6");
    }

    #[test]
    fn test_retain_recent_no_op_when_under_target() {
        let memory = ShortTermMemory::new(10);
        for i in 0..3 {
            memory.store(create_entry(&format!("entry-{i}")));
        }
        let removed = memory.retain_recent(5);
        assert_eq!(removed, 0);
        assert_eq!(memory.len(), 3);
    }

    #[test]
    fn test_retain_recent_clamps_to_capacity() {
        let memory = ShortTermMemory::new(5);
        for i in 0..5 {
            memory.store(create_entry(&format!("entry-{i}")));
        }
        // Target above capacity: capped, nothing dropped.
        let removed = memory.retain_recent(100);
        assert_eq!(removed, 0);
        assert_eq!(memory.len(), 5);
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

#[cfg(test)]
mod coverage_short_term_accessors {
    //! Small accessors on `ShortTermMemory` that the existing
    //! tests do not exercise: `is_empty`, `capacity`, and
    //! `get_recent` beyond the size limit. A regression here
    //! silently breaks a caller's sizing logic.
    use super::*;
    use kod_types::MemoryType;
    use time::OffsetDateTime;

    fn e(content: &str) -> MemoryEntry {
        MemoryEntry {
            id: MemoryId::new(),
            memory_type: MemoryType::ShortTerm,
            content: content.to_string(),
            timestamp: OffsetDateTime::now_utc(),
            relevance: 1.0,
            metadata: Default::default(),
        
            superseded_by: None,
            contradicts: Vec::new(),
        }
    }

    #[test]
    fn is_empty_matches_len_zero() {
        let m = ShortTermMemory::new(5);
        assert!(m.is_empty());
        assert_eq!(m.len(), 0);
        m.store(e("a"));
        assert!(!m.is_empty());
        assert_eq!(m.len(), 1);
        m.clear();
        assert!(m.is_empty());
    }

    #[test]
    fn capacity_reports_the_construction_argument() {
        assert_eq!(ShortTermMemory::new(42).capacity(), 42);
        assert_eq!(ShortTermMemory::new(0).capacity(), 0);
    }

    #[test]
    fn get_recent_beyond_len_returns_everything_in_order() {
        let m = ShortTermMemory::new(10);
        for i in 0..3 {
            m.store(e(&format!("v{i}")));
        }
        let got = m.get_recent(100);
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].content, "v0");
        assert_eq!(got[2].content, "v2");
    }

    #[test]
    fn get_recent_zero_returns_empty() {
        let m = ShortTermMemory::new(10);
        m.store(e("a"));
        assert!(m.get_recent(0).is_empty());
    }

    #[test]
    fn search_is_case_insensitive_substring() {
        let m = ShortTermMemory::new(10);
        m.store(e("Hello World"));
        m.store(e("goodbye"));
        assert_eq!(m.search("hello").len(), 1);
        assert_eq!(m.search("WORLD").len(), 1);
        assert_eq!(m.search("nope").len(), 0);
    }

    #[test]
    fn search_on_empty_memory_is_empty() {
        let m = ShortTermMemory::new(10);
        assert!(m.search("anything").is_empty());
    }

    #[test]
    fn get_all_returns_insertion_order() {
        let m = ShortTermMemory::new(10);
        for i in 0..5 {
            m.store(e(&format!("v{i}")));
        }
        let all = m.get_all();
        for (i, item) in all.iter().enumerate() {
            assert_eq!(item.content, format!("v{i}"));
        }
    }
}
