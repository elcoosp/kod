//! The engine's structured provider path (design §2 AD-01).
//!
//! `process_for` and its streaming sibling call
//! `LlmProvider::complete` / `LlmProvider::stream_completion` with a
//! `CompletionRequest`. This file proves that with a mock provider that
//! records which trait method was invoked, and inspects the request.
//!
//! # Why this is a separate file from the trait-contract suite
//!
//! The trait suite (`kod-provider::testkit::run_trait_contracts`)
//! exercises the trait's shape with a `MockProvider` directly. It does
//! not go through the engine. A regression in the *engine* — reverting
//! to `generate_with_tools`, forgetting to build the request, dropping
//! the user's turn — would leave every contract green and every prompt
//! wrong. This is the integration-side check that closes that gap.

use async_trait::async_trait;
use futures::Stream;
use kod_core::router::RouterConfig;
use kod_core::{KodEngine, TaskResponse};
use kod_error::Result;
use kod_provider::request::CompletionRequest;
use kod_provider::traits::GenerationOptions;
use kod_provider::{GenerationResponse, LlmProvider, ModelRef, ProviderCapabilities, StreamChunk};
use kod_types::ToolDefinition;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

/// One call to `complete()` or `stream_completion()`.
#[derive(Clone)]
struct Recorded {
    system_len: usize,
    messages_len: usize,
    /// The last message's role, as a debug string — used to assert the
    /// request ends on the user's turn (the shape a chat completion
    /// requires).
    last_role: String,
    /// The last message's content, so the test can assert the user's
    /// input made it through.
    last_content: String,
}

#[derive(Clone)]
struct Recorder {
    completes: Arc<Mutex<Vec<Recorded>>>,
    streams: Arc<Mutex<Vec<Recorded>>>,
    legacy_calls: Arc<Mutex<usize>>,
}

impl Recorder {
    fn new() -> Self {
        Self {
            completes: Arc::new(Mutex::new(Vec::new())),
            streams: Arc::new(Mutex::new(Vec::new())),
            legacy_calls: Arc::new(Mutex::new(0)),
        }
    }
}

struct StructuredProvider {
    recorder: Recorder,
}

#[async_trait]
impl LlmProvider for StructuredProvider {
    fn name(&self) -> &str {
        "structured-test"
    }

    async fn list_models(&self) -> Result<Vec<String>> {
        Ok(vec!["test-model".into()])
    }

    async fn generate(&self, _prompt: &str, _opts: &GenerationOptions) -> Result<String> {
        *self.recorder.legacy_calls.lock().unwrap() += 1;
        Ok("legacy path called".into())
    }

    async fn generate_with_tools(
        &self,
        _prompt: &str,
        _tools: &[ToolDefinition],
        _opts: &GenerationOptions,
    ) -> Result<GenerationResponse> {
        *self.recorder.legacy_calls.lock().unwrap() += 1;
        Ok(GenerationResponse::Text {
            content: "legacy path called".into(),
            usage: None,
        })
    }

    async fn complete(&self, req: &CompletionRequest) -> Result<GenerationResponse> {
        let last = req.messages.last();
        self.recorder.completes.lock().unwrap().push(Recorded {
            system_len: req.system.render_text().len(),
            messages_len: req.messages.len(),
            last_role: last
                .map(|m| format!("{:?}", m.role))
                .unwrap_or_else(|| "(none)".into()),
            last_content: last.map(|m| m.content.clone()).unwrap_or_default(),
        });
        Ok(GenerationResponse::Text {
            content: "structured reply".into(),
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

    fn stream_completion<'a>(
        &'a self,
        req: &'a CompletionRequest,
    ) -> Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send + 'a>> {
        let last = req.messages.last();
        self.recorder.streams.lock().unwrap().push(Recorded {
            system_len: req.system.render_text().len(),
            messages_len: req.messages.len(),
            last_role: last
                .map(|m| format!("{:?}", m.role))
                .unwrap_or_else(|| "(none)".into()),
            last_content: last.map(|m| m.content.clone()).unwrap_or_default(),
        });
        Box::pin(async_stream::stream! {
            yield Ok(StreamChunk::Text("streamed reply".into()));
            yield Ok(StreamChunk::Done);
        })
    }
}

async fn engine_with(recorder: Recorder) -> (TempDir, Arc<KodEngine>) {
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

    let mut registry = kod_provider::ProviderRegistry::new();
    registry.insert(
        "default",
        Arc::new(StructuredProvider { recorder }),
        ProviderCapabilities::conservative(),
        "test-model",
    );
    engine
        .set_registry(
            Arc::new(registry),
            ModelRef::new("default", "test-model"),
            None,
        )
        .await;

    engine.start().await.unwrap();
    (tmp, engine)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn process_calls_complete_not_generate_with_tools() {
    let recorder = Recorder::new();
    let (_tmp, engine) = engine_with(recorder.clone()).await;

    let response: TaskResponse = engine.process("hello provider").await.expect("process");

    let completes = recorder.completes.lock().unwrap().clone();
    assert!(
        !completes.is_empty(),
        "engine must call `LlmProvider::complete` on the structured path",
    );
    assert_eq!(
        *recorder.legacy_calls.lock().unwrap(),
        0,
        "engine must not call the legacy `generate_with_tools`/`generate` path",
    );
    assert_eq!(
        response.text.as_deref(),
        Some("structured reply"),
        "the response text must come from the structured call",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn complete_request_carries_system_and_user_turn() {
    let recorder = Recorder::new();
    let (_tmp, engine) = engine_with(recorder.clone()).await;

    engine.process("identify yourself").await.expect("process");

    let recorded = recorder
        .completes
        .lock()
        .unwrap()
        .last()
        .cloned()
        .expect("at least one `complete` call");

    assert!(
        recorded.system_len > 0,
        "the CompletionRequest's system prompt must not be empty \
         — the identity + environment grounding vanished",
    );
    assert!(
        recorded.messages_len > 0,
        "the CompletionRequest's messages must not be empty \
         — the user's turn was dropped",
    );
    assert_eq!(
        recorded.last_role, "User",
        "the last message must be the user's turn (chat completion shape)",
    );
    assert_eq!(
        recorded.last_content, "identify yourself",
        "the last message's content must be the user's input",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn process_streaming_calls_stream_completion() {
    let recorder = Recorder::new();
    let (_tmp, engine) = engine_with(recorder.clone()).await;

    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(16);
    // Drain the receiver in a background task so the engine never
    // blocks on a full channel.
    let drain = tokio::spawn(async move {
        let mut acc = String::new();
        while let Some(chunk) = rx.recv().await {
            acc.push_str(&chunk);
        }
        acc
    });

    let _ = engine
        .process_streaming("hello streamed", &tx)
        .await
        .expect("process_streaming");
    drop(tx);
    let _text = drain.await.expect("drain task");

    let streams = recorder.streams.lock().unwrap().clone();
    assert!(
        !streams.is_empty(),
        "engine must call `LlmProvider::stream_completion` on the streaming path",
    );
    let recorded = &streams[0];
    assert!(
        recorded.system_len > 0,
        "the streaming CompletionRequest's system prompt must not be empty",
    );
    assert_eq!(
        recorded.last_role, "User",
        "the streaming request must end on the user's turn",
    );
}

// Re-export `async_stream` for the stream_completion body above.
