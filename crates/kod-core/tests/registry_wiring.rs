//! Integration tests for engine registry wiring (A4b).
//!
//! The engine resolves providers through a `ProviderRegistry`
//! exclusively. The tests here cover:
//!
//! 1. A registry installs and serves prompts.
//! 2. `set_current_model` switches the resolved provider.
//! 3. `current_model` reflects both setters.

use async_trait::async_trait;
use futures::Stream;
use kod_core::{KodEngine, RouterConfig};
use kod_error::Result;
use kod_provider::{
    GenerationOptions, GenerationResponse, LlmProvider, ModelRef, ProviderRegistry, StreamChunk,
};
use kod_types::ToolDefinition;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

mod common;

/// A provider that records every prompt it sees and replies with a
/// text that identifies it — so a test can assert *which* provider
/// served a call.
struct NamedProvider {
    name: &'static str,
    calls: Mutex<usize>,
}

impl NamedProvider {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            calls: Mutex::new(0),
        }
    }
    fn calls(&self) -> usize {
        *self.calls.lock().unwrap()
    }
}

#[async_trait]
impl LlmProvider for NamedProvider {
    fn name(&self) -> &str {
        self.name
    }
    async fn list_models(&self) -> Result<Vec<String>> {
        Ok(vec![])
    }
    async fn generate(&self, _p: &str, _o: &GenerationOptions) -> Result<String> {
        *self.calls.lock().unwrap() += 1;
        Ok(self.name.to_string())
    }
    async fn generate_with_tools(
        &self,
        _p: &str,
        _t: &[ToolDefinition],
        _o: &GenerationOptions,
    ) -> Result<GenerationResponse> {
        *self.calls.lock().unwrap() += 1;
        Ok(GenerationResponse::Text {
            content: format!("reply from {}", self.name),
            usage: None,
        })
    }
    fn stream(
        &self,
        _p: &str,
        _o: &GenerationOptions,
    ) -> Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send + '_>> {
        Box::pin(futures::stream::empty())
    }
}

fn engine_in(dir: &std::path::Path) -> KodEngine {
    std::fs::write(dir.join("main.rs"), "pub fn main() {}\n").unwrap();
    let db_path = dir.join("test.redb");
    let cfg = RouterConfig {
        embedder: None, skill_threshold: 0.3,
        working_dir: dir.to_path_buf(),
        enable_memory: false,
        max_skills_per_query: 3,
        context_window: 8192,
        short_term_capacity: 100,
    };
    KodEngine::new(cfg, db_path).unwrap()
}



#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn set_current_model_switches_the_resolved_provider() {
    let temp = TempDir::new().unwrap();
    let engine = engine_in(temp.path());
    let a = Arc::new(NamedProvider::new("a"));
    let b = Arc::new(NamedProvider::new("b"));

    let mut registry = ProviderRegistry::new();
    registry.insert("a", a.clone(), kod_provider::ProviderCapabilities::conservative(), "any-model");
    registry.insert("b", b.clone(), kod_provider::ProviderCapabilities::conservative(), "any-model");
    engine
        .set_registry(Arc::new(registry), ModelRef::new("a", "m"), None)
        .await;
    engine.start().await.unwrap();

    engine.process("first").await.unwrap();
    assert_eq!(a.calls(), 1);
    assert_eq!(b.calls(), 0);

    engine.set_current_model(ModelRef::new("b", "m")).await;
    engine.process("second").await.unwrap();
    assert_eq!(a.calls(), 1, "provider 'a' should not be called again");
    assert_eq!(b.calls(), 1, "provider 'b' should now serve");

    engine.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn current_model_reflects_set_registry_and_set_current_model() {
    let temp = TempDir::new().unwrap();
    let engine = engine_in(temp.path());
    let a = Arc::new(NamedProvider::new("a"));
    let b = Arc::new(NamedProvider::new("b"));

    // Default before any set_registry is the initial value.
    let initial = engine.current_model().await;
    assert_eq!(initial.endpoint, "default");

    let mut registry = ProviderRegistry::new();
    registry.insert("a", a, kod_provider::ProviderCapabilities::conservative(), "any-model");
    registry.insert("b", b, kod_provider::ProviderCapabilities::conservative(), "any-model");
    engine
        .set_registry(Arc::new(registry), ModelRef::new("a", "m1"), None)
        .await;
    assert_eq!(engine.current_model().await, ModelRef::new("a", "m1"));

    engine.set_current_model(ModelRef::new("b", "m2")).await;
    assert_eq!(engine.current_model().await, ModelRef::new("b", "m2"));

    engine.shutdown().await.unwrap();
}
