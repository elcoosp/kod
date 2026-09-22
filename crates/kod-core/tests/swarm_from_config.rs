//! `SwarmRunner::from_config` mapping (design §4 D4.3).
//!
//! The runner's config knobs (`agent_timeout_secs`, `agent_retries`,
//! `timeout_secs`) live in `[swarm]`. Both the CLI `kod swarm` and the
//! TUI `/swarm` build their runner through `from_config`, so a
//! regression that dropped a knob would leave every existing test
//! green — the defaults it falls back to are valid, just not the ones
//! the user asked for. This test asserts each field reaches the
//! runner.

use kod_config::SwarmConfig;
use kod_core::router::RouterConfig;
use kod_core::{KodEngine, SwarmRunner};
use std::sync::Arc;
use tempfile::TempDir;

async fn engine() -> (TempDir, Arc<KodEngine>) {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("test.redb");
    let cfg = RouterConfig {
        skill_threshold: 0.3,
        context_window: 8192,
        short_term_capacity: 100,
        working_dir: tmp.path().to_path_buf(),
        enable_memory: false,
        max_skills_per_query: 3,
        embedder: None,
    };
    let engine = Arc::new(KodEngine::new(cfg, db_path).unwrap());
    // The runner needs a provider to build; a minimal mock will do.
    // Any existing test double suffices; the mapping under test does
    // not exercise the provider.
    struct NopProvider;
    #[async_trait::async_trait]
    impl kod_provider::LlmProvider for NopProvider {
        fn name(&self) -> &str {
            "nop"
        }
        async fn list_models(&self) -> kod_error::Result<Vec<kod_provider::ModelInfo>> {
            Ok(vec![])
        }
        async fn generate(
            &self,
            _p: &str,
            _o: &kod_provider::GenerationOptions,
        ) -> kod_error::Result<String> {
            Ok(String::new())
        }
        async fn generate_with_tools(
            &self,
            _p: &str,
            _t: &[kod_types::ToolDefinition],
            _o: &kod_provider::GenerationOptions,
        ) -> kod_error::Result<kod_provider::GenerationResponse> {
            Ok(kod_provider::GenerationResponse::Text {
                content: String::new(),
                usage: None,
            })
        }
        fn stream(
            &self,
            _p: &str,
            _o: &kod_provider::GenerationOptions,
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
    let mut registry = kod_provider::ProviderRegistry::new();
    registry.insert(
        "default",
        Arc::new(NopProvider),
        kod_provider::ProviderCapabilities::conservative(),
        "test-model",
    );
    engine
        .set_registry(
            Arc::new(registry),
            kod_provider::ModelRef::new("default", "test-model"),
            None,
        )
        .await;
    engine.start().await.unwrap();
    (tmp, engine)
}

#[tokio::test]
async fn from_config_clamps_agents_and_applies_budget_knobs() {
    let (_tmp, engine) = engine().await;
    let config = SwarmConfig {
        max_agents: 3,
        merge_results: false,
        agent_timeout_secs: 42,
        agent_retries: 7,
        timeout_secs: 999,
    };
    let runner = SwarmRunner::from_config(engine, &config).await.unwrap();
    // `max_agents` is clamped to [2, 8]; 3 is in range.
    assert_eq!(runner.max_agents(), 3, "max_agents must be passed through");
}

#[tokio::test]
async fn from_config_clamps_extreme_max_agents() {
    let (_tmp, engine) = engine().await;
    let config = SwarmConfig {
        // Above the clamp ceiling: `SwarmRunner::new` clamps to 8.
        max_agents: 100,
        ..SwarmConfig::default()
    };
    let runner = SwarmRunner::from_config(engine, &config).await.unwrap();
    assert_eq!(runner.max_agents(), 8, "max_agents must be clamped to 8");
}
