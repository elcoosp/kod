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

#[test]
fn test_remove() {
    let memory = ShortTermMemory::new(10);
    let entry = create_entry("test");
    memory.store(entry.clone());

    let removed = memory.remove(&entry.id);
    assert!(removed.is_some());
    assert_eq!(memory.len(), 0);
}

#[test]
fn test_search() {
    let memory = ShortTermMemory::new(10);

    memory.store(create_entry("Rust programming language"));
    memory.store(create_entry("Python scripting language"));
    memory.store(create_entry("JavaScript web development"));

    let results = memory.search("rust");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].content, "Rust programming language");
}

#[test]
fn test_clear() {
    let memory = ShortTermMemory::new(10);

    memory.store(create_entry("first"));
    memory.store(create_entry("second"));

    memory.clear();
    assert_eq!(memory.len(), 0);
    assert!(memory.is_empty());
}

#[test]
fn test_capacity() {
    let memory = ShortTermMemory::new(5);
    assert_eq!(memory.capacity(), 5);
}
