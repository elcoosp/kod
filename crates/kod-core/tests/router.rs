use kod_core::router::{RouterConfig, TaskRouter, TaskType};
use kod_types::MemoryContext;
use tempfile::TempDir;

fn create_test_router() -> (TaskRouter, TempDir) {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("test.redb");

    let router = TaskRouter::new(RouterConfig::default(), db_path).unwrap();

    (router, temp_dir)
}

#[tokio::test]
async fn test_classify_simple_task() {
    let (router, _temp) = create_test_router();

    let task_type = router.classify_task("What is 2 + 2?").await.unwrap();
    assert_eq!(task_type, TaskType::Simple);
}

#[tokio::test]
async fn test_classify_code_modification() {
    let (router, _temp) = create_test_router();

    let task_type = router
        .classify_task("Refactor the main function to use async")
        .await
        .unwrap();
    assert_eq!(task_type, TaskType::CodeModification);

    let task_type = router
        .classify_task("Fix the bug in auth handler")
        .await
        .unwrap();
    assert_eq!(task_type, TaskType::CodeModification);
}

#[tokio::test]
async fn test_classify_debugging() {
    let (router, _temp) = create_test_router();

    let task_type = router
        .classify_task("Debug this error: panic in main.rs")
        .await
        .unwrap();
    assert_eq!(task_type, TaskType::Debugging);

    let task_type = router
        .classify_task("Traceback: TypeError in line 42")
        .await
        .unwrap();
    assert_eq!(task_type, TaskType::Debugging);
}

#[tokio::test]
async fn test_classify_research() {
    let (router, _temp) = create_test_router();

    let task_type = router
        .classify_task("Research best practices for async Rust")
        .await
        .unwrap();
    assert_eq!(task_type, TaskType::Research);

    let task_type = router
        .classify_task("Find documentation for tokio runtime")
        .await
        .unwrap();
    assert_eq!(task_type, TaskType::Research);
}

#[tokio::test]
async fn test_classify_complex_task() {
    let (router, _temp) = create_test_router();

    let task_type = router
        .classify_task("Design and implement a complete authentication system")
        .await
        .unwrap();
    assert_eq!(task_type, TaskType::Complex);

    let task_type = router
        .classify_task("Analyze the architecture and plan refactoring")
        .await
        .unwrap();
    assert_eq!(TaskType::Complex, task_type);
}

#[tokio::test]
async fn test_classify_testing() {
    let (router, _temp) = create_test_router();

    let task_type = router
        .classify_task("Write unit tests for the auth module")
        .await
        .unwrap();
    assert_eq!(task_type, TaskType::Testing);
}

#[tokio::test]
async fn test_classify_documentation() {
    let (router, _temp) = create_test_router();

    let task_type = router
        .classify_task("Document the public API")
        .await
        .unwrap();
    assert_eq!(task_type, TaskType::Documentation);
}

#[tokio::test]
async fn test_classify_multi_step() {
    let (router, _temp) = create_test_router();

    let task_type = router
        .classify_task("First analyze the code, then implement changes, then test them")
        .await
        .unwrap();
    assert_eq!(task_type, TaskType::Complex);
}

#[tokio::test]
async fn test_route_simple_task() {
    let (router, _temp) = create_test_router();

    let response = router.process_input("What is Rust?").await.unwrap();

    // The router classifies and describes; it does not generate.
    // `text` is None here — the caller (the engine) fills it with the
    // provider's reply. Regression: the router used to return a
    // placeholder string ("Processing simple task: …"), which leaked
    // to any caller using the router directly.
    assert!(response.text.is_none(), "router must not fabricate reply text");
    assert!(response.tool_calls.is_empty());
    assert!(response.tool_results.is_empty());
    assert_eq!(response.task_type, TaskType::Simple);
}

#[tokio::test]
async fn test_route_with_context() {
    let (router, _temp) = create_test_router();

    // Build memory context
    let memory_context = MemoryContext {
        working_memory: vec![],
        long_term: vec![],
        total_tokens: 100,
    };

    let response = router
        .process_input_with_context("What is Rust?", Some(memory_context))
        .await
        .unwrap();

    // Same contract as test_route_simple_task: the router classifies
    // and reports what context it saw, it does not generate text.
    assert!(response.text.is_none());
    assert_eq!(response.task_type, TaskType::Simple);
    // An empty memory context contributes nothing, so `memory_used`
    // is false even though a context was passed in.
    assert!(!response.memory_used);
}

#[tokio::test]
async fn test_router_configuration() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("test.redb");

    let config = RouterConfig {
        embedder: None, skill_threshold: 0.3,
        context_window: 8192,
        short_term_capacity: 100,
        max_skills_per_query: 2,
        enable_memory: false,
        working_dir: temp_dir.path().to_path_buf(),
    };

    let router = TaskRouter::new(config, db_path).unwrap();

    // Should not use swarm or memory when disabled
    let response = router.process_input("Test input").await.unwrap();
    assert!(response.text.is_some() || response.tool_calls.is_empty());
}
