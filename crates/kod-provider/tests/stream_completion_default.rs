//! `LlmProvider::stream_completion`'s collect-and-replay default
//! (design §2 AD-01).
//!
//! The trait's `stream_completion` default calls `complete` and
//! replays the response through `response_chunks`. That is what makes
//! a provider which has not migrated still work when the engine calls
//! the structured streaming method — a mock, a plugin, a provider
//! implemented outside this workspace.
//!
//! The shared trait contract suite
//! (`kod_provider::testkit::run_trait_contracts`) also exercises this
//! path through a `MockProvider`. This file pins it from the
//! provider's own crate, at the point where the default is actually
//! defined, so a refactor that moved the default into `kod-core` or
//! `kod-tui` would still have a test that failed.

use async_trait::async_trait;
use futures::{Stream, StreamExt};
use kod_error::Result;
use kod_provider::request::{CompletionRequest, ModelRef, SystemPrompt};
use kod_provider::traits::{GenerationOptions, LlmProvider};
use kod_provider::{GenerationResponse, StreamChunk};
use kod_types::{ChatMessage, MessageId, MessageRole, ToolCall};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use time::OffsetDateTime;

/// A provider that implements only the collected path. It does not
/// override `stream_completion`, so calling that method exercises the
/// trait's default.
struct CollectedOnlyProvider {
    /// One scripted `GenerationResponse` per call. Replayed in order;
    /// the last entry is reused when the queue runs dry.
    responses: Mutex<Vec<GenerationResponse>>,
    /// Number of `complete` calls observed. The default should call
    /// `complete` exactly once per `stream_completion` — that is the
    /// point of the replay shape.
    complete_calls: Arc<Mutex<usize>>,
}

impl CollectedOnlyProvider {
    fn new(responses: Vec<GenerationResponse>) -> Self {
        Self {
            responses: Mutex::new(responses),
            complete_calls: Arc::new(Mutex::new(0)),
        }
    }

    fn next(&self) -> GenerationResponse {
        let mut q = self.responses.lock().unwrap();
        if q.len() > 1 {
            q.remove(0)
        } else {
            q[0].clone()
        }
    }
}

#[async_trait]
impl LlmProvider for CollectedOnlyProvider {
    fn name(&self) -> &str {
        "collected-only"
    }
    async fn list_models(&self) -> Result<Vec<String>> {
        Ok(vec![])
    }
    async fn generate(&self, _p: &str, _o: &GenerationOptions) -> Result<String> {
        Ok(String::new())
    }
    async fn generate_with_tools(
        &self,
        _p: &str,
        _t: &[kod_types::ToolDefinition],
        _o: &GenerationOptions,
    ) -> Result<GenerationResponse> {
        Ok(GenerationResponse::Text {
            content: String::new(),
            usage: None,
        })
    }
    async fn complete(&self, _req: &CompletionRequest) -> Result<GenerationResponse> {
        *self.complete_calls.lock().unwrap() += 1;
        Ok(self.next())
    }
    fn stream(
        &self,
        _p: &str,
        _o: &GenerationOptions,
    ) -> Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send + '_>> {
        Box::pin(futures::stream::empty())
    }
    // NOTE: `stream_completion` intentionally *not* overridden.
}

fn request() -> CompletionRequest {
    let mut req = CompletionRequest::new(ModelRef::new("test", "m"));
    req.system = SystemPrompt::new();
    req.messages = vec![ChatMessage::text(
        MessageId::new(),
        MessageRole::User,
        "hi",
        OffsetDateTime::now_utc(),
    )];
    req
}

#[tokio::test]
async fn text_response_replays_as_text_then_done() {
    let provider = CollectedOnlyProvider::new(vec![GenerationResponse::Text {
        content: "hello there".into(),
        usage: None,
    }]);
    let req = request();
    let mut stream = provider.stream_completion(&req);
    let mut text = String::new();
    let mut saw_done = false;
    while let Some(chunk) = stream.next().await {
        match chunk.expect("no errors on the happy path") {
            StreamChunk::Text(t) => text.push_str(&t),
            StreamChunk::Done => {
                saw_done = true;
                break;
            }
            _ => {}
        }
    }
    assert!(saw_done, "the default must yield `Done`");
    assert_eq!(text, "hello there");
    assert_eq!(
        *provider.complete_calls.lock().unwrap(),
        1,
        "the default must call `complete` exactly once",
    );
}

#[tokio::test]
async fn tool_call_response_replays_as_indexed_start_and_delta() {
    let call = ToolCall {
        id: Some("call_1".into()),
        tool_name: "read_file".into(),
        arguments: serde_json::json!({"path": "a.rs"}),
    };
    let provider = CollectedOnlyProvider::new(vec![GenerationResponse::ToolCalls {
        calls: vec![call],
        usage: None,
    }]);
    let req = request();
    let mut stream = provider.stream_completion(&req);
    let mut start = None;
    let mut delta = None;
    let mut saw_done = false;
    while let Some(chunk) = stream.next().await {
        match chunk.expect("no errors") {
            StreamChunk::ToolCallStart { index, id, name } => {
                start = Some((index, id, name));
            }
            StreamChunk::ToolCallDelta { index, arguments } => {
                delta = Some((index, arguments));
            }
            StreamChunk::Done => {
                saw_done = true;
                break;
            }
            _ => {}
        }
    }
    assert!(saw_done, "the default must yield `Done`");
    let (idx, id, name) = start.expect("one ToolCallStart");
    assert_eq!(idx, 0, "single call uses index 0");
    assert_eq!(id.as_deref(), Some("call_1"), "id must round-trip");
    assert_eq!(name, "read_file");
    let (didx, args) = delta.expect("one ToolCallDelta");
    assert_eq!(didx, 0);
    // The delta is the serialized arguments; the engine re-parses it.
    let parsed: serde_json::Value = serde_json::from_str(&args).expect("delta is JSON");
    assert_eq!(parsed["path"], "a.rs");
}

#[tokio::test]
async fn error_surfaces_as_one_err_chunk() {
    // A provider that returns an error from `complete` must surface it
    // through the stream, not silently swallow it.
    struct ErrProvider;
    #[async_trait]
    impl LlmProvider for ErrProvider {
        fn name(&self) -> &str {
            "err"
        }
        async fn list_models(&self) -> Result<Vec<String>> {
            Ok(vec![])
        }
        async fn generate(&self, _p: &str, _o: &GenerationOptions) -> Result<String> {
            Ok(String::new())
        }
        async fn generate_with_tools(
            &self,
            _p: &str,
            _t: &[kod_types::ToolDefinition],
            _o: &GenerationOptions,
        ) -> Result<GenerationResponse> {
            Ok(GenerationResponse::Text {
                content: String::new(),
                usage: None,
            })
        }
        async fn complete(&self, _req: &CompletionRequest) -> Result<GenerationResponse> {
            Err(kod_error::KodError::Provider("scripted failure".into()))
        }
        fn stream(
            &self,
            _p: &str,
            _o: &GenerationOptions,
        ) -> Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send + '_>> {
            Box::pin(futures::stream::empty())
        }
    }

    let provider = ErrProvider;
    let req = request();
    let mut stream = provider.stream_completion(&req);
    let mut saw_err = false;
    while let Some(chunk) = stream.next().await {
        if chunk.is_err() {
            saw_err = true;
            break;
        }
    }
    assert!(saw_err, "a `complete` error must surface as an `Err` chunk");
}
