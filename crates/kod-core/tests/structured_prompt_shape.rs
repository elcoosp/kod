//! The structured prompt reaching the provider (design §2 AD-01, AD-16).
//!
//! Two facts that the existing suites do not assert:
//!
//! 1. The system prompt a provider receives does **not** contain the
//!    user's request or the transcript. The router's rendered plan
//!    ends with a transcript block; `build_grounded_request` strips it
//!    before the request reaches the wire. Without the strip the model
//!    sees the user's turn twice per call.
//!
//! 2. The `messages` array carries the user's turn as the last entry,
//!    so the request ends on a `User` — the shape a chat-completions
//!    API requires.
//!
//! Both facts are observed from the mock provider's `complete()`,
//! which records what the engine actually built.

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
    system: String,
    last_message_role: String,
    last_message_content: String,
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
        let system = req.system.render_text();
        let last = req.messages.last();
        self.snapshots.lock().unwrap().push(Snapshot {
            system,
            last_message_role: last.map(|m| format!("{:?}", m.role)).unwrap_or_default(),
            last_message_content: last.map(|m| m.content.clone()).unwrap_or_default(),
        });
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

/// The user's request must reach the provider exactly once — in the
/// `messages` array, not duplicated in the system prompt. A regression
/// that dropped `strip_conversation_tail` would put the request in
/// both places; a regression that dropped the history push would put
/// it in neither.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn user_turn_appears_once_in_the_structured_request() {
    let provider = Arc::new(RecordingProvider {
        snapshots: Arc::new(Mutex::new(Vec::new())),
    });
    let (_tmp, engine) = engine_with_provider(provider.clone()).await;

    // A distinctive marker so a substring search is unambiguous.
    let marker = "zz-unique-user-request-marker-zz";
    engine.process(marker).await.expect("process");

    let snaps = provider.snapshots.lock().unwrap().clone();
    assert!(!snaps.is_empty(), "engine must call `complete`");
    let snap = snaps.last().unwrap();

    assert!(
        !snap.system.contains(marker),
        "the user's request must not leak into the system prompt — \
         `strip_conversation_tail` did not remove the tail. System was:\n{}",
        snap.system,
    );
    assert!(
        !snap.system.contains("## User Request"),
        "the router's user-request header must be stripped from the system prompt",
    );
    assert!(
        !snap.system.contains("## Conversation so far"),
        "the router's conversation header must be stripped from the system prompt",
    );

    // And yet the user's turn must reach the provider as the last
    // message.
    assert_eq!(
        snap.last_message_role, "User",
        "the last message must be the user's turn",
    );
    assert_eq!(snap.last_message_content, marker);
}

/// On a turn where the transcript has one prior exchange, the system
/// prompt still has no transcript, but the messages array grows by
/// the new user turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn second_turn_does_not_leak_history_into_system() {
    let provider = Arc::new(RecordingProvider {
        snapshots: Arc::new(Mutex::new(Vec::new())),
    });
    let (_tmp, engine) = engine_with_provider(provider.clone()).await;

    engine.process("first question").await.expect("turn 1");
    engine.process("second question").await.expect("turn 2");

    let snaps = provider.snapshots.lock().unwrap().clone();
    assert!(snaps.len() >= 2, "at least two provider calls");
    let turn2 = snaps.last().unwrap();

    // The prior turn's content must not appear in the system prompt.
    assert!(
        !turn2.system.contains("first question"),
        "the previous turn must not leak into the system prompt via \
         the transcript section",
    );
    // The second user turn is the last message.
    assert_eq!(turn2.last_message_content, "second question");
}
