//! Per-capability model routing (design §4 D1.4 PR A7).
//!
//! `KodEngine::resolve_model_ref_for_capability` maps a swarm
//! `Capability` to a `ModelRef` via `[llm.routing.swarm]`. The engine's
//! `build_streaming_chain` uses that override when present; when the
//! table is missing or has no entry for a capability, it returns `None`
//! and the engine falls back to task-type routing — the pre-A7 shape.
//!
//! This file pins the mapping in both directions: an entry yields the
//! matching endpoint+model, and a missing entry yields `None` (never a
//! wrong endpoint).

use kod_config::RoutingConfig;
use kod_core::KodEngine;
use kod_core::router::RouterConfig;
use kod_provider::{ModelRef, ProviderCapabilities};
use std::sync::Arc;
use tempfile::TempDir;

/// A provider stub; the mapping under test does not call it.
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
        Box<dyn futures::Stream<Item = kod_error::Result<kod_provider::StreamChunk>> + Send + '_>,
    > {
        Box::pin(futures::stream::empty())
    }
}

async fn engine_with_routing(routing: Option<RoutingConfig>) -> (TempDir, Arc<KodEngine>) {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("t.redb");
    let cfg = RouterConfig {
        skill_threshold: 0.3,
        context_window: 8192,
        short_term_capacity: 100,
        working_dir: tmp.path().to_path_buf(),
        enable_memory: false,
        max_skills_per_query: 3,
        embedder: None,
    };
    let engine = Arc::new(KodEngine::new(cfg, db).unwrap());

    let mut registry = kod_provider::ProviderRegistry::new();
    registry.insert(
        "local-ollama",
        Arc::new(NopProvider),
        ProviderCapabilities::conservative(),
        "qwen2.5-coder:7b",
    );
    registry.insert(
        "anthropic",
        Arc::new(NopProvider),
        ProviderCapabilities::conservative(),
        "claude-sonnet-4-5",
    );
    engine
        .set_registry(
            Arc::new(registry),
            ModelRef::new("local-ollama", "qwen2.5-coder:7b"),
            routing,
        )
        .await;
    engine.start().await.unwrap();
    (tmp, engine)
}

#[tokio::test]
async fn coding_capability_routes_to_local_endpoint() {
    let mut routing = RoutingConfig::default();
    routing.swarm.insert("coding".into(), "local-ollama".into());
    routing
        .swarm
        .insert("code-review".into(), "anthropic".into());
    let (_tmp, engine) = engine_with_routing(Some(routing)).await;

    let coding = engine
        .resolve_model_ref_for_capability(&kod_swarm::Capability::Coding)
        .await
        .expect("coding must map to an endpoint");
    assert_eq!(coding.endpoint, "local-ollama");
    assert_eq!(coding.model, "qwen2.5-coder:7b");

    let review = engine
        .resolve_model_ref_for_capability(&kod_swarm::Capability::CodeReview)
        .await
        .expect("code-review must map to an endpoint");
    assert_eq!(review.endpoint, "anthropic");
    assert_eq!(review.model, "claude-sonnet-4-5");
}

#[tokio::test]
async fn missing_capability_entry_returns_none() {
    let mut routing = RoutingConfig::default();
    routing.swarm.insert("coding".into(), "local-ollama".into());
    // No entry for `Testing` — the caller should get `None` and fall
    // back to task-type routing, not a wrong endpoint.
    let (_tmp, engine) = engine_with_routing(Some(routing)).await;
    let testing = engine
        .resolve_model_ref_for_capability(&kod_swarm::Capability::Testing)
        .await;
    assert!(testing.is_none());
}

#[tokio::test]
async fn no_routing_table_returns_none_for_every_capability() {
    // A v1-style config with no `[llm.routing.swarm]`: every capability
    // resolves to `None`, meaning the engine falls back to task-type
    // routing — the pre-A7 behaviour.
    let (_tmp, engine) = engine_with_routing(None).await;
    for cap in [
        kod_swarm::Capability::Coding,
        kod_swarm::Capability::Testing,
        kod_swarm::Capability::CodeReview,
    ] {
        assert!(
            engine
                .resolve_model_ref_for_capability(&cap)
                .await
                .is_none(),
            "no routing table must mean `None` for {cap:?}",
        );
    }
}

#[tokio::test]
async fn unknown_endpoint_name_returns_none() {
    // A routing entry that names an endpoint not in the registry is
    // dropped by the config validator in practice, but the engine
    // must still not panic — it must return `None` so the caller
    // falls back.
    let mut routing = RoutingConfig::default();
    routing
        .swarm
        .insert("coding".into(), "nonexistent-endpoint".into());
    let (_tmp, engine) = engine_with_routing(Some(routing)).await;
    let result = engine
        .resolve_model_ref_for_capability(&kod_swarm::Capability::Coding)
        .await;
    assert!(
        result.is_none(),
        "an entry pointing at an unregistered endpoint must yield None",
    );
}
