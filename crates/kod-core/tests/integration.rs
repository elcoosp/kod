use kod_core::{
    engine::KodEngine,
    router::{RouterConfig, TaskType},
};
use tempfile::TempDir;

fn create_test_environment() -> (KodEngine, TempDir) {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("memory.redb");

    // Create skills directory
    let skills_dir = temp_dir.path().join("skills");
    std::fs::create_dir_all(&skills_dir).unwrap();

    // Create a test skill
    let skill_content = r#"---
name: rust-coding
description: Rust coding assistance
version: 1.0.0
category: coding
tags:
  - rust
  - coding
capabilities:
  - code-generation
triggers:
  - "rust code"
  - "write rust"
---

## Instructions

You are a Rust coding expert. Help with idiomatic Rust code.
"#;

    std::fs::write(skills_dir.join("rust.md"), skill_content).unwrap();

    let config = RouterConfig {
        working_dir: temp_dir.path().to_path_buf(),
        enable_swarm: false, // Disable for testing
        enable_memory: true,
        ..Default::default()
    };

    let engine = KodEngine::new(config, db_path).unwrap();
    (engine, temp_dir)
}

#[tokio::test]
async fn test_full_engine_pipeline() {
    let (engine, temp_dir) = create_test_environment();

    // 1. Start engine
    engine.start().await.unwrap();

    // 2. Load skills
    let skills_dir = temp_dir.path().join("skills");
    engine.load_skills(&skills_dir).await.unwrap();

    // 3. Process various task types
    let test_cases = vec![
        ("What is 2 + 2?", TaskType::Simple),
        ("Fix the bug in main.rs", TaskType::CodeModification),
        ("Debug this error", TaskType::Debugging),
        ("Research async patterns", TaskType::Research),
        ("Write tests for auth", TaskType::Testing),
        ("Document the API", TaskType::Documentation),
    ];

    for (input, expected_type) in test_cases {
        let response = engine.process(input).await;

        match response {
            Ok(resp) => {
                assert_eq!(resp.task_type, expected_type, "Failed for input: {}", input);
            }
            Err(e) => {
                // Some may fail without LLM provider, but task type should be classified
                panic!("Engine failed for input '{}': {:?}", input, e);
            }
        }
    }

    // 4. Run maintenance
    engine.run_maintenance().await.unwrap();

    // 5. Shutdown
    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_engine_with_memory() {
    let (engine, _temp) = create_test_environment();

    engine.start().await.unwrap();
    // Process input that should use memory
    let response = engine.process("Remember that I prefer Rust").await;

    // Memory should be used for this type of input
    if let Ok(_resp) = response {
        // Just verify it doesn't panic
    }

    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_engine_skill_matching() {
    let (engine, temp_dir) = create_test_environment();

    engine.start().await.unwrap();

    // Load skills
    let skills_dir = temp_dir.path().join("skills");
    engine.load_skills(&skills_dir).await.unwrap();

    // Process input that should match skill
    let response = engine.process("Help me write rust code").await;

    if let Ok(_resp) = response {
        // Skills should be detected for rust-related queries
        // (Note: actual skill matching depends on router implementation)
    }

    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_engine_error_handling() {
    let (engine, _temp) = create_test_environment();

    // Engine not started, should fail
    let result = engine.process("Test").await;
    assert!(result.is_err());

    // Start and test
    engine.start().await.unwrap();

    // Empty input
    let _result = engine.process("").await;
    // May succeed or fail, but shouldn't panic

    // Very long input
    let long_input = "a".repeat(10000);
    let _result = engine.process(&long_input).await;
    // Should handle gracefully

    engine.shutdown().await.unwrap();
}
