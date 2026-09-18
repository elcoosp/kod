//! Fallback chain integration test (A6, AD-06/07).
//!
//! Two providers: `primary` always fails with a retryable error;
//! `secondary` replies successfully. With the config routing `Simple`
//! to `primary` and listing `secondary` in `fallback`, a `Simple`
//! prompt must be served by `secondary`, and a
//! `SessionEntry::ModelFallback` line must land in the JSONL session
//! log.

use async_trait::async_trait;
use futures::Stream;
use kod_core::{KodEngine, RouterConfig};
use kod_error::{KodError, Result};
use kod_provider::{
    GenerationOptions, GenerationResponse, LlmProvider, ModelRef, ProviderRegistry,
    StreamChunk,
};
use kod_types::ToolDefinition;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

struct FailingProvider {
    calls: Mutex<usize>,
}

impl FailingProvider {
    fn new() -> Self {
        Self {
            calls: Mutex::new(0),
        }
    }
    fn calls(&self) -> usize {
        *self.calls.lock().unwrap()
    }
}

#[async_trait]
impl LlmProvider for FailingProvider {
    fn name(&self) -> &str {
        "failing"
    }
    async fn list_models(&self) -> Result<Vec<String>> {
        Ok(vec![])
    }
    async fn generate(&self, _p: &str, _o: &GenerationOptions) -> Result<String> {
        *self.calls.lock().unwrap() += 1;
        Err(KodError::Provider("503 service unavailable".into()))
    }
    async fn generate_with_tools(
        &self,
        _p: &str,
        _t: &[ToolDefinition],
        _o: &GenerationOptions,
    ) -> Result<GenerationResponse> {
        *self.calls.lock().unwrap() += 1;
        Err(KodError::Provider("503 service unavailable".into()))
    }
    fn stream(
        &self,
        _p: &str,
        _o: &GenerationOptions,
    ) -> Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send + '_>> {
        Box::pin(futures::stream::empty())
    }
}

struct SuccessProvider {
    calls: Mutex<usize>,
}

impl SuccessProvider {
    fn new() -> Self {
        Self {
            calls: Mutex::new(0),
        }
    }
    fn calls(&self) -> usize {
        *self.calls.lock().unwrap()
    }
}

#[async_trait]
impl LlmProvider for SuccessProvider {
    fn name(&self) -> &str {
        "success"
    }
    async fn list_models(&self) -> Result<Vec<String>> {
        Ok(vec![])
    }
    async fn generate(&self, _p: &str, _o: &GenerationOptions) -> Result<String> {
        *self.calls.lock().unwrap() += 1;
        Ok("ok".into())
    }
    async fn generate_with_tools(
        &self,
        _p: &str,
        _t: &[ToolDefinition],
        _o: &GenerationOptions,
    ) -> Result<GenerationResponse> {
        *self.calls.lock().unwrap() += 1;
        Ok(GenerationResponse::Text {
            content: "fallback reply".into(),
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
async fn retryable_error_falls_back_to_next_endpoint() {
    let temp = TempDir::new().unwrap();
    let engine = engine_in(temp.path());

    let primary = Arc::new(FailingProvider::new());
    let secondary = Arc::new(SuccessProvider::new());

    let mut registry = ProviderRegistry::new();
    registry.insert(
        "primary",
        primary.clone(),
        kod_provider::ProviderCapabilities::conservative(),
        "m-primary",
    );
    registry.insert(
        "secondary",
        secondary.clone(),
        kod_provider::ProviderCapabilities::conservative(),
        "m-secondary",
    );

    // Route Simple -> primary, fallback to secondary.
    let mut routing = kod_config::RoutingConfig::default();
    routing.by_task.insert("Simple".to_string(), "primary".to_string());
    routing.fallback.push("secondary".to_string());

    engine
        .set_registry(
            Arc::new(registry),
            ModelRef::new("primary", "m-primary"),
            Some(routing),
        )
        .await;

    // Attach a session log to the temp dir so we can assert on it.
    let log_path = temp.path().join("session.jsonl");
    let recorder = kod_core::session_log::SessionRecorder::open(log_path.clone()).unwrap();
    engine.set_session_recorder(Arc::new(recorder));

    engine.start().await.unwrap();
    let resp = engine.process("hello").await.unwrap();
    assert_eq!(resp.text.as_deref(), Some("fallback reply"));

    // Primary was tried once, secondary served the reply.
    assert_eq!(primary.calls(), 1, "primary must be attempted");
    assert_eq!(secondary.calls(), 1, "secondary must serve the reply");

    // The JSONL log must contain exactly one ModelFallback line
    // recording the primary -> secondary transition.
    let entries = kod_core::session_log::read_session(&log_path).unwrap();
    let fallbacks: Vec<_> = entries
        .iter()
        .filter_map(|e| match e {
            kod_core::session_log::SessionEntry::ModelFallback {
                from, to, error, ..
            } => Some((from.clone(), to.clone(), error.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(fallbacks.len(), 1, "expected exactly one fallback log entry");
    let (from, to, err) = &fallbacks[0];
    assert!(from.contains("primary"), "from: {from}");
    assert!(to.contains("secondary"), "to: {to}");
    assert!(err.contains("503"), "error: {err}");

    engine.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_retryable_error_does_not_fall_back() {
    let temp = TempDir::new().unwrap();
    let engine = engine_in(temp.path());

    // A provider that always fails with a non-retryable error.
    struct AuthFailingProvider;
    #[async_trait]
    impl LlmProvider for AuthFailingProvider {
        fn name(&self) -> &str {
            "auth-failing"
        }
        async fn list_models(&self) -> Result<Vec<String>> {
            Ok(vec![])
        }
        async fn generate(&self, _p: &str, _o: &GenerationOptions) -> Result<String> {
            Err(KodError::Provider("401 unauthorized".into()))
        }
        async fn generate_with_tools(
            &self,
            _p: &str,
            _t: &[ToolDefinition],
            _o: &GenerationOptions,
        ) -> Result<GenerationResponse> {
            Err(KodError::Provider("401 unauthorized".into()))
        }
        fn stream(
            &self,
            _p: &str,
            _o: &GenerationOptions,
        ) -> Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send + '_>> {
            Box::pin(futures::stream::empty())
        }
    }

    let primary = Arc::new(AuthFailingProvider);
    let secondary = Arc::new(SuccessProvider::new());

    let mut registry = ProviderRegistry::new();
    registry.insert(
        "primary",
        primary.clone(),
        kod_provider::ProviderCapabilities::conservative(),
        "m-primary",
    );
    registry.insert(
        "secondary",
        secondary.clone(),
        kod_provider::ProviderCapabilities::conservative(),
        "m-secondary",
    );

    let mut routing = kod_config::RoutingConfig::default();
    routing.by_task.insert("Simple".to_string(), "primary".to_string());
    routing.fallback.push("secondary".to_string());

    engine
        .set_registry(
            Arc::new(registry),
            ModelRef::new("primary", "m-primary"),
            Some(routing),
        )
        .await;
    engine.start().await.unwrap();

    let err = engine.process("hello").await.unwrap_err();
    assert!(
        err.to_string().contains("401"),
        "auth error must propagate unchanged: {err}"
    );
    assert_eq!(
        secondary.calls(),
        0,
        "non-retryable errors must not trigger fallback"
    );

    engine.shutdown().await.unwrap();
}
