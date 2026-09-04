use kod_memory::episodic::EpisodicMemory;
use kod_memory::manager::MemoryManager;
use kod_types::{EpisodicMemory as EpisodicMemoryType, MemoryId, MemoryType, Outcome};
use tempfile::TempDir;

fn create_episode(content: &str, task_type: &str, outcome: Outcome) -> EpisodicMemoryType {
    EpisodicMemoryType {
        id: MemoryId::new(),
        content: content.to_string(),
        embedding: vec![0.1, 0.2, 0.3],
        task_type: task_type.to_string(),
        outcome,
        timestamp: time::OffsetDateTime::now_utc(),
    }
}

#[tokio::test]
async fn test_episodic_store_and_get() {
    let memory = EpisodicMemory::new();
    let episode = create_episode("Fixed a bug", "debugging", Outcome::Success);

    memory.store(episode.clone()).await.unwrap();

    let retrieved = memory.get(&episode.id).await.unwrap();
    assert!(retrieved.is_some());
    assert_eq!(retrieved.unwrap().content, "Fixed a bug");
}

#[tokio::test]
async fn test_episodic_find_similar() {
    let memory = EpisodicMemory::new();

    let ep1 = create_episode("Fixed login bug", "debugging", Outcome::Success);
    let ep2 = create_episode("Fixed payment bug", "debugging", Outcome::Success);
    let ep3 = create_episode("Wrote new feature", "coding", Outcome::Success);

    memory.store(ep1.clone()).await.unwrap();
    memory.store(ep2.clone()).await.unwrap();
    memory.store(ep3).await.unwrap();

    // Search using ep1's embedding
    let similar = memory
        .find_similar(&ep1.embedding, 2)
        .await
        .unwrap();

    assert_eq!(similar.len(), 2);
    // ep1 should be the most similar (identical embedding)
    assert_eq!(similar[0].id, ep1.id);
}

#[tokio::test]
async fn test_episodic_by_task_type() {
    let memory = EpisodicMemory::new();

    memory
        .store(create_episode("Debug 1", "debugging", Outcome::Success))
        .await
        .unwrap();
    memory
        .store(create_episode("Debug 2", "debugging", Outcome::Failure))
        .await
        .unwrap();
    memory
        .store(create_episode("Write code", "coding", Outcome::Success))
        .await
        .unwrap();

    let debugging = memory.get_by_task_type("debugging").await.unwrap();
    assert_eq!(debugging.len(), 2);

    let failures = memory.get_by_outcome(Outcome::Failure).await.unwrap();
    assert_eq!(failures.len(), 1);
}

#[tokio::test]
async fn test_memory_manager_integration() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("test.redb");

    let manager = MemoryManager::new(db_path, 5).unwrap();

    // Store in short-term
    let short_id = manager
        .store(MemoryType::ShortTerm, "Working on task")
        .await
        .unwrap();

    // Store in long-term
    let long_id = manager
        .store(MemoryType::LongTerm, "Important knowledge")
        .await
        .unwrap();

    // Store episodic
    let _epi_id = manager
        .store(MemoryType::Episodic, "Completed task successfully")
        .await
        .unwrap();

    // Verify retrievals
    assert!(manager.get_short_term(&short_id).is_some());
    assert!(manager.get_long_term(&long_id).await.unwrap().is_some());

    // Search
    let results = manager.search("Important").await.unwrap();
    assert!(results.iter().any(|e| e.content.contains("Important")));

    // Context retrieval
    let context = manager.retrieve_context("task").await.unwrap();
    assert!(!context.working_memory.is_empty());
}
