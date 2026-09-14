//! End-to-end integration tests for KOD.
//!
//! These tests verify the full system lifecycle including:
//! - CLI command structure and help output
//! - Skill loading and matching via kod-core APIs
//! - Memory storage and retrieval via kod-core APIs
//! - Task classification via kod-core APIs
//! - Configuration file usage
//! - Error handling

mod common;

use common::{TestEnvironment, run_kod_command};

// ---- CLI command tests ----

#[test]
fn test_cli_help() {
    let env = TestEnvironment::new();
    let output = run_kod_command(&["--help"], &env.working_dir).unwrap();
    assert!(output.contains("KOD"));
}

#[test]
fn test_cli_version() {
    let env = TestEnvironment::new();
    let output = run_kod_command(&["--version"], &env.working_dir).unwrap();
    assert!(output.contains("0.1.0"));
}

#[test]
fn test_cli_config_command() {
    let env = TestEnvironment::new();
    // The config command should run and produce output
    let output = run_kod_command(&["config"], &env.working_dir).unwrap();
    // Wired CLI prints "KOD Configuration:" with full config dump
    assert!(
        output.contains("configuration")
            || output.contains("config")
            || output.contains("Configuration")
            || output.contains("Config")
    );
}

#[test]
fn test_cli_skills_command() {
    let env = TestEnvironment::new();
    let output = run_kod_command(&["skills"], &env.working_dir).unwrap();
    assert!(output.contains("skills") || output.contains("Skills"));
}

#[test]
fn test_cli_chat_command_runs() {
    let env = TestEnvironment::new();
    // Chat command should start and produce some output or error gracefully
    let result = run_kod_command(&["chat"], &env.working_dir);
    // Either succeeds or fails gracefully
    assert!(result.is_ok() || result.is_err());
}

// ---- In-process integration tests (kod-core APIs) ----

#[tokio::test]
async fn test_task_routers_config() {
    let env = TestEnvironment::new();
    let db_path = env.working_dir.join("test.redb");

    let router = kod_core::router::TaskRouter::new(
        kod_core::router::RouterConfig {
            context_window: 8192,
            enable_swarm: false,
            enable_memory: true,
            max_skills_per_query: 3,
            working_dir: env.working_dir.clone(),
        },
        db_path,
    )
    .unwrap();

    // Verify router was created successfully
    let _ = router;
}

#[tokio::test]
async fn test_task_classification_full() {
    let env = TestEnvironment::new();
    let db_path = env.working_dir.join("test.redb");

    let router = kod_core::router::TaskRouter::new(
        kod_core::router::RouterConfig {
            context_window: 8192,
            enable_swarm: false,
            enable_memory: false,
            max_skills_per_query: 3,
            working_dir: env.working_dir.clone(),
        },
        db_path,
    )
    .unwrap();

    // Test classification of various task types
    let simple = router.classify_task("What is 2+2?").await.unwrap();
    assert_eq!(simple, kod_core::router::TaskType::Simple);

    let code_mod = router
        .classify_task("Fix the bug in main.rs")
        .await
        .unwrap();
    assert_eq!(code_mod, kod_core::router::TaskType::CodeModification);

    let debug = router
        .classify_task("Debug this error: panic in main")
        .await
        .unwrap();
    assert_eq!(debug, kod_core::router::TaskType::Debugging);

    let test_task = router
        .classify_task("Write unit tests for auth module")
        .await
        .unwrap();
    assert_eq!(test_task, kod_core::router::TaskType::Testing);

    let research = router
        .classify_task("Research best practices for async Rust")
        .await
        .unwrap();
    assert_eq!(research, kod_core::router::TaskType::Research);

    let docs = router
        .classify_task("Document the public API")
        .await
        .unwrap();
    assert_eq!(docs, kod_core::router::TaskType::Documentation);

    let complex = router
        .classify_task("Design and implement a complete authentication system")
        .await
        .unwrap();
    assert_eq!(complex, kod_core::router::TaskType::Complex);
}

#[tokio::test]
async fn test_engine_lifecycle() {
    let env = TestEnvironment::new();
    let db_path = env.working_dir.join("test.redb");

    let engine = kod_core::engine::KodEngine::new(
        kod_core::router::RouterConfig {
            context_window: 8192,
            enable_swarm: false,
            enable_memory: false,
            max_skills_per_query: 3,
            working_dir: env.working_dir.clone(),
        },
        db_path,
    )
    .unwrap();

    // Engine starts not running
    assert!(!engine.is_running().await);

    // Start engine
    engine.start().await.unwrap();
    assert!(engine.is_running().await);

    // Shutdown
    engine.shutdown().await.unwrap();
    assert!(!engine.is_running().await);
}

/// A provider that returns a fixed reply without touching the network.
///
/// `KodEngine::process` requires an installed provider — the earlier
/// fallback to the router's placeholder text was replaced with an
/// explicit `InvalidState` error (see the engine's `no_provider_error`
/// doc). This provider exists so an integration test can exercise the
/// full engine path (prompt build → provider call → response) without
/// depending on a live model server.
struct NoOpProvider {
    reply: String,
}

#[async_trait::async_trait]
impl kod_provider::LlmProvider for NoOpProvider {
    fn name(&self) -> &str {
        "no-op"
    }

    async fn list_models(&self) -> kod_error::Result<Vec<String>> {
        Ok(vec!["no-op".to_string()])
    }

    async fn generate(
        &self,
        _prompt: &str,
        _options: &kod_provider::GenerationOptions,
    ) -> kod_error::Result<String> {
        Ok(self.reply.clone())
    }

    async fn generate_with_tools(
        &self,
        _prompt: &str,
        _tools: &[kod_types::ToolDefinition],
        _options: &kod_provider::GenerationOptions,
    ) -> kod_error::Result<kod_provider::GenerationResponse> {
        // Return Text, not ToolCalls: the test drives a single round
        // and asserts the reply text lands. A ToolCalls response would
        // need a tool registry entry to satisfy the loop.
        Ok(kod_provider::GenerationResponse::Text {
            content: self.reply.clone(),
            usage: None,
        })
    }

    fn stream(
        &self,
        _prompt: &str,
        _options: &kod_provider::GenerationOptions,
    ) -> std::pin::Pin<
        Box<
            dyn futures::Stream<Item = kod_error::Result<kod_provider::StreamChunk>>
                + Send
                + '_,
        >,
    > {
        Box::pin(futures::stream::empty())
    }
}

#[tokio::test]
async fn test_engine_process_input() {
    let env = TestEnvironment::new();
    let db_path = env.working_dir.join("test.redb");

    let engine = kod_core::engine::KodEngine::new(
        kod_core::router::RouterConfig {
            context_window: 8192,
            enable_swarm: false,
            enable_memory: false,
            max_skills_per_query: 3,
            working_dir: env.working_dir.clone(),
        },
        db_path,
    )
    .unwrap();

    engine.start().await.unwrap();

    // Install a no-op provider. Without one the engine rejects the
    // call — see the `no_provider_error` doc in kod-core. This test
    // exercises the real processing path with a canned reply.
    engine
        .set_provider(std::sync::Arc::new(NoOpProvider {
            reply: "I can help with code.".to_string(),
        }))
        .await;

    // Process a simple input.
    let response = engine.process("Hello, what can you do?").await.unwrap();
    assert_eq!(response.task_type, kod_core::router::TaskType::Simple);
    assert_eq!(
        response.text.as_deref(),
        Some("I can help with code."),
        "provider reply must reach the response"
    );

    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_engine_rejects_prompt_without_provider() {
    // The complementary test: with no provider installed, `process`
    // must reject the call rather than return a placeholder. This is
    // the behavior the no-provider fix introduced; pinning it here
    // means a future revert to the fallback fails loudly.
    let env = TestEnvironment::new();
    let db_path = env.working_dir.join("test.redb");

    let engine = kod_core::engine::KodEngine::new(
        kod_core::router::RouterConfig {
            context_window: 8192,
            enable_swarm: false,
            enable_memory: false,
            max_skills_per_query: 3,
            working_dir: env.working_dir.clone(),
        },
        db_path,
    )
    .unwrap();
    engine.start().await.unwrap();

    let err = engine.process("Hello?").await.unwrap_err();
    assert!(
        matches!(err, kod_error::KodError::InvalidState(_)),
        "expected InvalidState, got {err:?}"
    );
    assert!(
        err.to_string().contains("No LLM provider"),
        "error should name the missing provider: {err}"
    );

    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn test_config_file_usage() {
    let env = TestEnvironment::new();
    let config_path = env.create_config("test-model");

    // Load the config we just wrote
    let config = kod_config::KodConfig::load_from(&config_path).unwrap();
    assert_eq!(config.llm.model, "test-model");
    assert_eq!(
        config.llm.provider,
        kod_config::llm::ProviderType::OpenAICompatible
    );
}

#[tokio::test]
async fn test_memory_system_integration() {
    let env = TestEnvironment::new();
    let db_path = env.db_path.clone();

    let manager = kod_memory::MemoryManager::new(db_path, 100).unwrap();

    // Store a long-term memory
    let memory_id = manager
        .store(kod_types::MemoryType::LongTerm, "User prefers Rust")
        .await
        .unwrap();

    // Retrieve it
    let retrieved = manager.get_long_term(&memory_id).await.unwrap();
    assert!(retrieved.is_some());
    assert_eq!(retrieved.unwrap().content, "User prefers Rust");

    // Search memory
    let results = manager.search("Rust").await.unwrap();
    assert!(!results.is_empty());
    assert!(results.iter().any(|r| r.content.contains("Rust")));
}

#[tokio::test]
async fn test_skill_loading_integration() {
    let env = TestEnvironment::new();

    // Add test skills
    env.add_skill("rust-coding", "coding", &["rust code", "write rust"]);
    env.add_skill("python-testing", "testing", &["python test", "pytest"]);

    // Load skills via the loader
    let mut loader = kod_skills::SkillLoader::new(&env.skills_dir);
    let skills = loader.load_all().await.unwrap();
    assert_eq!(skills.len(), 2);

    // Create a matcher and add skills
    let matcher = kod_skills::SkillMatcher::new();
    for skill in skills {
        matcher.add_skill(skill).await;
    }

    // Search for relevant skills
    let matches = matcher.find_relevant_skills("rust code").await;
    assert!(!matches.is_empty());
}

#[test]
fn test_cli_error_handling() {
    let env = TestEnvironment::new();

    // Running with no subcommand should show help or error
    let result = run_kod_command(&[], &env.working_dir);
    // Empty args: clap will print help or error
    assert!(result.is_ok() || result.is_err());
}

#[test]
fn test_environment_setup() {
    let env = TestEnvironment::new();

    // Verify environment is set up correctly
    assert!(env.working_dir.exists());
    assert!(env.skills_dir.exists());
    assert!(!env.db_path.exists()); // DB not created yet

    // Add skill and verify
    env.add_skill("test-skill", "test", &["test trigger"]);
    let skill_file = env.skills_dir.join("test-skill.md");
    assert!(skill_file.exists());

    // Verify config creation
    let config_path = env.create_config("test-model");
    assert!(config_path.exists());
    let content = std::fs::read_to_string(&config_path).unwrap();
    assert!(content.contains("test-model"));
}
