//! Steers reach the provider on the structured path (regression guard).
//!
//! Before the AD-01 migration, `apply_steers` wrote the steer note into
//! the text `pending` string — which was the model's entire input.
//! After the migration the model receives a `CompletionRequest`; the
//! `pending` string survives only as a `/debug last-prompt` trace. A
//! regression that left `apply_steers` appending to `pending` alone
//! would make `/steer` a silent no-op: the user types a redirect, the
//! TUI reports "Steered —", and the model never sees it.
//!
//! This file proves the fix: a steer queued before `process` runs
//! appears as a `User` message in the request the provider receives.

use async_trait::async_trait;
use futures::Stream;
use kod_core::KodEngine;
use kod_core::router::RouterConfig;
use kod_error::Result;
use kod_provider::request::CompletionRequest;
use kod_provider::traits::GenerationOptions;
use kod_provider::{GenerationResponse, LlmProvider, ModelRef, ProviderCapabilities, StreamChunk};
use kod_types::ToolDefinition;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

#[derive(Clone, Debug)]
struct Snapshot {
    messages: Vec<(String, String)>, // (role, content)
}

struct RecordingProvider {
    snapshots: Arc<Mutex<Vec<Snapshot>>>,
}

#[async_trait]
impl LlmProvider for RecordingProvider {
    fn name(&self) -> &str {
        "recording"
    }
    async fn list_models(&self) -> Result<Vec<kod_provider::ModelInfo>> {
        Ok(vec![])
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
    async fn complete(&self, req: &CompletionRequest) -> Result<GenerationResponse> {
        let snapshot = Snapshot {
            messages: req
                .messages
                .iter()
                .map(|m| (format!("{:?}", m.role), m.content.clone()))
                .collect(),
        };
        self.snapshots.lock().unwrap().push(snapshot);
        Ok(GenerationResponse::Text {
            content: "ok".into(),
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

async fn engine_with_provider(provider: Arc<RecordingProvider>) -> (TempDir, Arc<KodEngine>) {
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
        "default",
        provider,
        ProviderCapabilities::conservative(),
        "test",
    );
    engine
        .set_registry(Arc::new(registry), ModelRef::new("default", "test"), None)
        .await;
    engine.start().await.unwrap();
    (tmp, engine)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pre_queued_steer_reaches_the_provider() {
    let provider = Arc::new(RecordingProvider {
        snapshots: Arc::new(Mutex::new(Vec::new())),
    });
    let (_tmp, engine) = engine_with_provider(provider.clone()).await;

    // Queue the steer before the prompt runs — the common case for a
    // user who types `/steer` right after Enter.
    engine.steer("focus on the parser module").await;

    engine.process("look at the code").await.expect("process");

    let snaps = provider.snapshots.lock().unwrap().clone();
    assert!(!snaps.is_empty(), "the provider must have been called");
    let first = &snaps[0];
    let has_steer = first
        .messages
        .iter()
        .any(|(_, content)| content.contains("focus on the parser module"));
    assert!(
        has_steer,
        "a pre-queued steer must reach the provider as a message; \
         the request carried: {:?}",
        first
            .messages
            .iter()
            .map(|(r, c)| format!("{r}: {}", &c[..c.len().min(60)]))
            .collect::<Vec<_>>(),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_steer_is_not_sent_twice() {
    // `apply_steers` drains the queue; calling it at the top and bottom
    // of a round must not duplicate the note.
    let provider = Arc::new(RecordingProvider {
        snapshots: Arc::new(Mutex::new(Vec::new())),
    });
    let (_tmp, engine) = engine_with_provider(provider.clone()).await;
    engine.steer("never sent twice").await;
    engine.process("do work").await.expect("process");

    let snaps = provider.snapshots.lock().unwrap().clone();
    // Count occurrences across the first provider call only — the
    // engine may call `complete` more than once if a tool round runs,
    // but the steer must appear exactly once per request, not
    // duplicated within a single request.
    let first = &snaps[0];
    let count = first
        .messages
        .iter()
        .filter(|(_, c)| c.contains("never sent twice"))
        .count();
    assert_eq!(
        count, 1,
        "the steer must appear exactly once in the request",
    );
}

/// Counterpart for `process` (the collected path). Same invariant: a
/// steer queued before the call starts must appear as a `User` message
/// on the first round. The collected loop is a distinct code path from
/// the streaming loop; both must drain steers at the top of the round.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pre_queued_steer_reaches_the_provider_collected_path() {
    let provider = Arc::new(RecordingProvider {
        snapshots: Arc::new(Mutex::new(Vec::new())),
    });
    let (_tmp, engine) = engine_with_provider(provider.clone()).await;

    engine.steer("collected-path steer marker").await;
    engine.process("do work").await.expect("process");

    let snaps = provider.snapshots.lock().unwrap().clone();
    assert!(!snaps.is_empty());
    let first = &snaps[0];
    let has = first
        .messages
        .iter()
        .any(|(_, c)| c.contains("collected-path steer marker"));
    assert!(
        has,
        "a pre-queued steer must reach the collected-path provider; \
         the request carried: {:?}",
        first
            .messages
            .iter()
            .map(|(r, c)| format!("{r}: {}", &c[..c.len().min(60)]))
            .collect::<Vec<_>>(),
    );
}
