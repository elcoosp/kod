use kod_core::context::{EngineContext, EngineContextBuilder};
use kod_types::MemoryContext;
use std::path::PathBuf;

#[test]
fn test_context_creation() {
    let context = EngineContext::new("test input");

    assert_eq!(context.user_input, "test input");
    assert!(context.memory_context.working_memory.is_empty());
    assert!(context.skills.is_empty());
}

#[test]
fn test_context_builder() {
    let memory_context = MemoryContext {
        working_memory: vec![],
        long_term: vec![],
        episodic: vec![],
        total_tokens: 0,
    };

    let context = EngineContextBuilder::new()
        .with_user_input("Test input")
        .with_memory(memory_context)
        .with_working_dir("/tmp/test")
        .build();

    assert_eq!(context.user_input, "Test input");
    assert_eq!(context.working_dir, PathBuf::from("/tmp/test"));
}

#[test]
fn test_context_serialization() {
    let context = EngineContext::new("Test input");

    let json = serde_json::to_string(&context).unwrap();
    assert!(json.contains("Test input"));

    let deserialized: EngineContext = serde_json::from_str(&json).unwrap();
    assert_eq!(deserialized.user_input, "Test input");
}

#[test]
fn test_context_to_prompt() {
    let mut context = EngineContext::new("How do I implement auth?");

    // Add memory context
    context.memory_context.working_memory.push(kod_types::MemoryEntry {
        id: kod_types::MemoryId::new(),
        memory_type: kod_types::MemoryType::ShortTerm,
        content: "User is working on auth system".to_string(),
        timestamp: time::OffsetDateTime::now_utc(),
        relevance: 1.0,
        metadata: Default::default(),
    });

    let prompt = context.to_prompt();

    assert!(prompt.contains("How do I implement auth?"));
    assert!(prompt.contains("User is working on auth system"));
    assert!(prompt.contains("## User Request"));
}
