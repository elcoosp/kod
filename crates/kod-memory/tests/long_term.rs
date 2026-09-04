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
    }
}

#[tokio::test]
async fn test_store_and_retrieve() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("test.redb");

    let memory = LongTermMemory::new(&db_path).unwrap();
    let entry = create_entry("Important fact");

    memory.store(entry.clone()).await.unwrap();

    let retrieved = memory.get(&entry.id).await.unwrap();
    assert!(retrieved.is_some());
    assert_eq!(retrieved.unwrap().content, "Important fact");
}

#[tokio::test]
async fn test_search() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("test.redb");

    let memory = LongTermMemory::new(&db_path).unwrap();

    memory.store(create_entry("Rust is a systems language")).await.unwrap();
    memory.store(create_entry("Python is a scripting language")).await.unwrap();

    let results = memory.search("rust").await.unwrap();
    assert_eq!(results.len(), 1);
    assert!(results[0].content.contains("Rust"));
}

#[tokio::test]
async fn test_remove() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("test.redb");

    let memory = LongTermMemory::new(&db_path).unwrap();
    let entry = create_entry("To be removed");

    memory.store(entry.clone()).await.unwrap();
    memory.remove(&entry.id).await.unwrap();

    let retrieved = memory.get(&entry.id).await.unwrap();
    assert!(retrieved.is_none());
}

#[tokio::test]
async fn test_count_and_clear() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("test.redb");

    let memory = LongTermMemory::new(&db_path).unwrap();

    memory.store(create_entry("fact 1")).await.unwrap();
    memory.store(create_entry("fact 2")).await.unwrap();

    assert_eq!(memory.count().await.unwrap(), 2);

    memory.clear().await.unwrap();
    assert_eq!(memory.count().await.unwrap(), 0);
}

#[tokio::test]
async fn test_persistence() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("test.redb");

    let entry = create_entry("Persistent fact");

    {
        let memory = LongTermMemory::new(&db_path).unwrap();
        memory.store(entry.clone()).await.unwrap();
    }

    // Reopen database
    let memory = LongTermMemory::new(&db_path).unwrap();
    let retrieved = memory.get(&entry.id).await.unwrap();
    assert!(retrieved.is_some());
}
