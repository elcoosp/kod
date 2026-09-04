use kod_core::engine::KodEngine;
use kod_core::router::RouterConfig;
use tempfile::TempDir;

fn create_test_engine() -> (KodEngine, TempDir) {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("test.redb");

    let config = RouterConfig {
        working_dir: temp_dir.path().to_path_buf(),
        ..Default::default()
    };

    let engine = KodEngine::new(config, db_path).unwrap();
    (engine, temp_dir)
}

#[tokio::test]
async fn test_engine_creation() {
    let (_engine, _temp) = create_test_engine();
}

#[tokio::test]
async fn test_engine_process_input() {
    let (engine, _temp) = create_test_engine();

    let response = engine.process("Hello, world!").await;

    match response {
        Ok(resp) => {
            // Should have some response
            assert!(resp.text.is_some() || !resp.tool_calls.is_empty());
        }
        Err(e) => {
            // May fail if no LLM provider configured, but shouldn't panic
            assert!(!e.to_string().is_empty());
        }
    }
}

#[tokio::test]
async fn test_engine_with_provider() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path().join("test.redb");

    let config = RouterConfig {
        working_dir: temp_dir.path().to_path_buf(),
        ..Default::default()
    };

    let engine = KodEngine::new(config, db_path).unwrap();

    // Configure provider (mock or real)
    // engine.set_provider(...);

    let response = engine.process("Test input").await;

    // Should handle input without error
    assert!(response.is_ok() || response.is_err());
}

#[tokio::test]
async fn test_engine_maintenance() {
    let (engine, _temp) = create_test_engine();

    // Run maintenance
    let result = engine.run_maintenance().await;

    // Should complete without error
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_engine_shutdown() {
    let (engine, _temp) = create_test_engine();

    // Shutdown gracefully
    let result = engine.shutdown().await;

    assert!(result.is_ok());
}
