//! M-16 regression: a failed endpoint attempt must not leave its tool
//! rounds in the shared transcript.
//!
//! The fallback chain walks endpoints until one succeeds. Pre-fix, each
//! attempt persisted its tool rounds into `self.history` as it went, so
//! a retryable failure left the dead attempt's rounds behind and the
//! winning endpoint stacked its own on top — duplicate rounds the
//! provider can reject. The fix snapshots the holder's history length
//! above the endpoint walk and truncates back to it per attempt.

use async_trait::async_trait;
use futures::Stream;
use kod_core::KodEngine;
use kod_core::router::RouterConfig;
use kod_error::{KodError, Result};
use kod_provider::request::CompletionRequest;
use kod_provider::traits::GenerationOptions;
use kod_provider::{
    GenerationResponse, LlmProvider, ModelRef, ProviderCapabilities, ProviderRegistry, StreamChunk,
};
use kod_types::{MessageRole, ToolCall, ToolDefinition};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use tempfile::TempDir;

/// Endpoint "a" answers the first call with a tool round, the second
/// with a retryable transport error. Endpoint "b" always answers text.
struct ChainProvider {
    endpoint: String,
    calls: Mutex<usize>,
}

impl ChainProvider {
    fn new(endpoint: &str) -> Self {
        Self {
            endpoint: endpoint.to_string(),
            calls: Mutex::new(0),
        }
    }
}

#[async_trait]
impl LlmProvider for ChainProvider {
    fn name(&self) -> &str {
        &self.endpoint
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities::conservative()
    }

    async fn list_models(&self) -> Result<Vec<kod_provider::ModelInfo>> {
        Ok(vec![kod_provider::ModelInfo::bare("test-model")])
    }

    async fn generate(&self, _p: &str, _o: &GenerationOptions) -> Result<String> {
        Ok(String::new())
    }

    async fn generate_with_tools(
        &self,
        _p: &str,
        _t: &[ToolDefinition],
        _o: &GenerationOptions,
    ) -> Result<GenerationResponse> {
        Ok(GenerationResponse::Text {
            content: String::new(),
            usage: None,
        })
    }

    async fn complete(&self, _req: &CompletionRequest) -> Result<GenerationResponse> {
        let mut n = self.calls.lock().unwrap();
        *n += 1;
        if self.endpoint == "a" {
            if *n == 1 {
                // One tool round, so the loop persists a round's worth
                // of messages into the shared history.
                Ok(GenerationResponse::ToolCalls {
                    calls: vec![ToolCall {
                        id: Some("c1".to_string()),
                        tool_name: "list_files".to_string(),
                        arguments: serde_json::json!({ "path": "." }),
                    }],
                    usage: None,
                })
            } else {
                // Retryable transport failure — triggers the fallback.
                Err(KodError::Provider(
                    "connection reset by peer (transient)".to_string(),
                ))
            }
        } else {
            Ok(GenerationResponse::Text {
                content: "done".to_string(),
                usage: None,
            })
        }
    }

    fn stream(
        &self,
        _p: &str,
        _o: &GenerationOptions,
    ) -> Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send + '_>> {
        Box::pin(futures::stream::empty())
    }

    fn stream_completion<'a>(
        &'a self,
        _req: &'a CompletionRequest,
    ) -> Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send + 'a>> {
        Box::pin(futures::stream::empty())
    }
}

async fn engine_with_chain() -> (TempDir, Arc<KodEngine>) {
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

    let mut registry = ProviderRegistry::new();
    registry.insert(
        "a",
        Arc::new(ChainProvider::new("a")),
        ProviderCapabilities::conservative(),
        "test-model",
    );
    registry.insert(
        "b",
        Arc::new(ChainProvider::new("b")),
        ProviderCapabilities::conservative(),
        "test-model",
    );

    // Primary "a" for every task type, fallback to "b". Covering all
    // labels keeps the chain [a, b] regardless of how the input is
    // classified.
    let mut by_task = std::collections::BTreeMap::new();
    for label in [
        "Simple",
        "CodeModification",
        "Debugging",
        "Research",
        "Testing",
        "Documentation",
        "Complex",
        "MultiStep",
    ] {
        by_task.insert(label.to_string(), "a".to_string());
    }
    let routing = kod_config::RoutingConfig {
        by_task,
        fallback: vec!["b".to_string()],
        ..Default::default()
    };

    engine
        .set_registry(
            Arc::new(registry),
            ModelRef::new("a", "test-model"),
            Some(routing),
        )
        .await;
    engine.start().await.unwrap();
    (tmp, engine)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_endpoint_does_not_leave_its_round_in_the_transcript() {
    let (_tmp, engine) = engine_with_chain().await;

    let response = engine.process("do the thing").await.expect("process");
    assert_eq!(
        response.text.as_deref(),
        Some("done"),
        "the fallback endpoint must serve the winning reply",
    );

    let history = engine.history_for("").await;
    let tool_rounds = history
        .iter()
        .filter(|m| matches!(m.role, MessageRole::Tool))
        .count();
    assert_eq!(
        tool_rounds, 0,
        "endpoint a's tool round must not survive into the transcript; \
         history roles: {:?}",
        history.iter().map(|m| format!("{:?}", m.role)).collect::<Vec<_>>(),
    );
}
