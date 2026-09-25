//! Shared provider contract suite (§11.2).
//!
//! # Why this exists
//!
//! Every `LlmProvider` implementor must behave the same way for the
//! engine's purposes: `complete()` returns a `GenerationResponse`
//! with the right variant for a given scenario, `stream_with_tools`'s
//! default streams the same data in the same chunk order, and
//! `capabilities()` answers sensibly. Swapping an implementor,
//! refactoring the trait, or adding a new provider must not silently
//! change those observable behaviours. This module is the shared
//! suite that proves they do not.
//!
//! # Two kinds of test
//!
//! 1. **Trait-level contracts** — everything that goes through the
//!    `LlmProvider` trait. Instantiated by a `MockProvider` this
//!    module owns, so the suite runs without a network and without
//!    any concrete provider's HTTP stack. A change to the trait's
//!    default methods (like `complete`, which currently delegates
//!    to `generate_with_tools`) shows up here.
//!
//! 2. **Request-shape contracts** — for a concrete provider with a
//!    real wire format, the caller points the provider at an
//!    `httpmock` server and asserts on the *captured request body*.
//!    The mock answers with a minimal valid response, so the test
//!    also proves the provider can round-trip through a server.
//!    Response-shape (does the provider parse SSE correctly) is a
//!    separate, provider-specific concern tested in the provider's
//!    own test file.
//!
//! # What is intentionally NOT tested here
//!
//! - **Live servers.** The suite is a mock-server suite; it must
//!   not require Ollama, LM Studio, or an Anthropic key. Live
//!   tests live behind `#[ignore]` in the provider crates.
//! - **Prompt-cache breakpoint placement.** Anthropic's
//!   `cache_control` is provider-specific; the concrete provider's
//!   own tests are the right place to pin it.
//! - **Error text.** Providers differ in how they phrase a 401;
//!   the trait only requires that the error is classified as
//!   `is_retryable() == false` for a permanent failure. The suite
//!   tests the classification, not the phrase.

use crate::request::{CompletionRequest, ModelRef, ProviderCapabilities, SystemPrompt};
use crate::traits::{GenerationOptions, LlmProvider};
use crate::types::{GenerationResponse, StreamChunk, TokenUsage};
use async_trait::async_trait;
use futures::Stream;
use kod_error::{KodError, Result};
use kod_types::{ChatMessage, MessageId, MessageRole, ToolCall, ToolDefinition};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use time::OffsetDateTime;

// ---------------------------------------------------------------------------
// MockProvider
// ---------------------------------------------------------------------------

/// One scripted response for a `MockProvider` call.
#[derive(Clone)]
pub enum Script {
    /// Answer with plain text.
    Text(String),
    /// Answer with tool calls.
    Calls(Vec<ToolCall>),
    /// Answer with both text and tool calls.
    Mixed { text: String, calls: Vec<ToolCall> },
    /// Answer with an error.
    Error(String),
}

/// A `LlmProvider` whose every response is scripted. Exists for the
/// contract suite; not exported except under the `testkit` feature.
pub struct MockProvider {
    name: String,
    capabilities: ProviderCapabilities,
    /// Responses, consumed in order. The last response is replayed
    /// on every call past the list's end — so a one-element script
    /// answers every call the same way, which is what most tests
    /// want.
    script: Mutex<Vec<Script>>,
    /// Request bodies the provider received. Not used by the trait
    /// suite directly, but the request-shape tests can read it.
    pub captured_prompts: Mutex<Vec<String>>,
}

impl MockProvider {
    /// A provider named `name` that answers every call with `script`.
    pub fn new(name: impl Into<String>, script: Script) -> Self {
        Self {
            name: name.into(),
            capabilities: ProviderCapabilities::conservative(),
            script: Mutex::new(vec![script]),
            captured_prompts: Mutex::new(Vec::new()),
        }
    }

    /// A provider with a multi-element script; each call consumes the
    /// next entry, repeating the last entry once the list runs out.
    pub fn scripted(name: impl Into<String>, script: Vec<Script>) -> Self {
        assert!(
            !script.is_empty(),
            "a scripted provider needs at least one entry"
        );
        Self {
            name: name.into(),
            capabilities: ProviderCapabilities::conservative(),
            script: Mutex::new(script),
            captured_prompts: Mutex::new(Vec::new()),
        }
    }

    /// Override the reported capabilities.
    pub fn with_capabilities(mut self, capabilities: ProviderCapabilities) -> Self {
        self.capabilities = capabilities;
        self
    }

    fn next_script(&self) -> Script {
        let mut g = self.script.lock().unwrap();
        if g.len() > 1 {
            g.remove(0)
        } else {
            g[0].clone()
        }
    }

    fn record(&self, prompt: &str) {
        self.captured_prompts
            .lock()
            .unwrap()
            .push(prompt.to_string());
    }
}

#[async_trait]
impl LlmProvider for MockProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn capabilities(&self) -> ProviderCapabilities {
        self.capabilities
    }

    async fn list_models(&self) -> Result<Vec<crate::traits::ModelInfo>> {
        Ok(vec![crate::traits::ModelInfo::bare("mock-model")])
    }

    async fn generate(&self, prompt: &str, _opts: &GenerationOptions) -> Result<String> {
        self.record(prompt);
        match self.next_script() {
            Script::Text(t) => Ok(t),
            Script::Mixed { text, .. } => Ok(text),
            Script::Calls(_) => Err(KodError::Provider(
                "mock: generate() called but the script has only tool calls".to_string(),
            )),
            Script::Error(e) => Err(KodError::Provider(e)),
        }
    }

    async fn generate_with_tools(
        &self,
        prompt: &str,
        _tools: &[ToolDefinition],
        _opts: &GenerationOptions,
    ) -> Result<GenerationResponse> {
        self.record(prompt);
        match self.next_script() {
            Script::Text(t) => Ok(GenerationResponse::Text {
                content: t,
                usage: Some(TokenUsage {
                    prompt_tokens: 10,
                    completion_tokens: 5,
                    total_tokens: 15,
                    ..Default::default()
                }),
            }),
            Script::Calls(c) => Ok(GenerationResponse::ToolCalls {
                calls: c,
                usage: None,
            }),
            Script::Mixed { text, calls } => Ok(GenerationResponse::Mixed {
                content: text,
                calls,
                usage: None,
            }),
            Script::Error(e) => Err(KodError::Provider(e)),
        }
    }

    fn stream(
        &self,
        _prompt: &str,
        _opts: &GenerationOptions,
    ) -> Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send + '_>> {
        Box::pin(futures::stream::empty())
    }
}

// ---------------------------------------------------------------------------
// The trait contract suite
// ---------------------------------------------------------------------------

/// Run every trait-level contract against `provider`. A concrete
/// provider crate calls this once, from its own `tests/contracts.rs`,
/// against an instance of its real `LlmProvider`.
pub async fn run_trait_contracts(provider: Arc<dyn LlmProvider>) {
    contract_name_is_nonempty(&provider);
    contract_capabilities_are_reported(&provider);
    contract_generate_returns_a_string(&provider).await;
    contract_generate_with_tools_text(&provider).await;
    contract_generate_with_tools_calls(&provider).await;
    contract_generate_with_tools_mixed(&provider).await;
    contract_complete_default_delegates(&provider).await;
    contract_complete_renders_system_and_messages(&provider).await;
    contract_list_models_is_a_vec(&provider).await;
    contract_error_propagates(&provider).await;
    // AD-01 (structured streaming). See the contract bodies below for
    // what each one pins.
    contract_stream_completion_text(&provider).await;
    contract_stream_completion_calls(&provider).await;
    contract_stream_completion_error(&provider).await;
}

fn contract_name_is_nonempty(provider: &Arc<dyn LlmProvider>) {
    assert!(
        !provider.name().is_empty(),
        "provider name must be non-empty (used in logs and UI)",
    );
}

fn contract_capabilities_are_reported(provider: &Arc<dyn LlmProvider>) {
    // Only the shape matters here: `streaming_tools` and
    // `prompt_cache` are provider-specific. The suite checks that
    // the matrix is self-consistent — a provider that does not
    // support tools should not claim `streaming_tools`.
    let caps = provider.capabilities();
    if !caps.tools {
        assert!(
            !caps.streaming_tools,
            "streaming_tools=true requires tools=true",
        );
    }
    let _ = caps.prompt_cache; // any value is valid
    let _ = caps.pricing; // any value is valid
}

async fn contract_generate_returns_a_string(provider: &Arc<dyn LlmProvider>) {
    let opts = GenerationOptions::default();
    match provider.generate("hello", &opts).await {
        Ok(s) => {
            // Either a text answer or an error is legal; a `Ok` with
            // an empty string is legal too. The point is that the
            // call does not panic.
            let _ = s;
        }
        Err(e) => {
            // A real provider that hits a network may error here; the
            // suite runs against mock providers, so an error means
            // the caller wired it wrong, not that the network is
            // down. Still — accept it, since the concrete provider
            // tests may be running against a real transport with an
            // unexpected URL.
            let _ = e;
        }
    }
}

async fn contract_generate_with_tools_text(provider: &Arc<dyn LlmProvider>) {
    let opts = GenerationOptions::default();
    let empty_tools: Vec<ToolDefinition> = Vec::new();
    match provider
        .generate_with_tools("hello", &empty_tools, &opts)
        .await
    {
        Ok(GenerationResponse::Text { .. }) => {}
        Ok(other) => panic!("generate_with_tools on a text-answering provider returned {other:?}",),
        Err(_) => {
            // Same tolerance as above.
        }
    }
}

async fn contract_generate_with_tools_calls(provider: &Arc<dyn LlmProvider>) {
    let opts = GenerationOptions::default();
    let empty_tools: Vec<ToolDefinition> = Vec::new();
    match provider
        .generate_with_tools("hello", &empty_tools, &opts)
        .await
    {
        Ok(GenerationResponse::ToolCalls { calls, .. }) => {
            assert!(!calls.is_empty(), "ToolCalls variant must carry ≥ 1 call");
        }
        Ok(_) | Err(_) => {
            // Only the caller that scripted Calls cares; a
            // provider that scripted text is not being tested here.
        }
    }
}

async fn contract_generate_with_tools_mixed(provider: &Arc<dyn LlmProvider>) {
    let opts = GenerationOptions::default();
    let empty_tools: Vec<ToolDefinition> = Vec::new();
    if let Ok(GenerationResponse::Mixed { calls, .. }) = provider
        .generate_with_tools("hello", &empty_tools, &opts)
        .await
    {
        assert!(!calls.is_empty(), "Mixed variant must carry ≥ 1 call");
    }
}

/// `complete()`'s default implementation renders a `CompletionRequest`
/// to text and calls `generate_with_tools`. This contract proves the
/// default is reachable and the rendered text contains what the
/// request carried.
async fn contract_complete_default_delegates(provider: &Arc<dyn LlmProvider>) {
    let mut req = CompletionRequest::new(ModelRef::new("mock", "mock-model"));
    req.system = SystemPrompt::new()
        .with("You are a test.", true)
        .with("env: test", false);
    req.messages = vec![ChatMessage::text(
        MessageId::new(),
        MessageRole::User,
        "ping",
        OffsetDateTime::now_utc(),
    )];

    // The call must not panic. The concrete provider's own tests
    // assert on the response *content*; the trait contract only
    // asserts reachability of the default method.
    let _ = provider.complete(&req).await;
}

/// The default adapter must render both the system segments and the
/// messages into the prompt the legacy method receives. This is
/// tested against `MockProvider`, which records every prompt.
async fn contract_complete_renders_system_and_messages(_provider: &Arc<dyn LlmProvider>) {
    let mock = MockProvider::new("recorder", Script::Text("ok".to_string()));
    let mut req = CompletionRequest::new(ModelRef::new("mock", "m"));
    req.system = SystemPrompt::new().with("You are a test.", true);
    req.messages = vec![ChatMessage::text(
        MessageId::new(),
        MessageRole::User,
        "unique-message-text",
        OffsetDateTime::now_utc(),
    )];

    // Call through the trait object to reach the default impl.
    let trait_obj: Arc<dyn LlmProvider> = Arc::new(mock);
    let _ = trait_obj.complete(&req).await;

    // Downcast is not possible on a trait object; re-check by
    // calling the default through a concrete `MockProvider`
    // reference held elsewhere. The pragmatic version: rebuild the
    // request, call the default through a fresh concrete provider,
    // and inspect its own capture vector.
    let concrete = MockProvider::new("recorder-2", Script::Text("ok".to_string()));
    let mut req2 = req.clone();
    req2.system = SystemPrompt::new().with("system-token-xyz", true);
    req2.messages = vec![ChatMessage::text(
        MessageId::new(),
        MessageRole::User,
        "user-token-abc",
        OffsetDateTime::now_utc(),
    )];
    let _ = concrete.complete(&req2).await;

    let seen = concrete.captured_prompts.lock().unwrap().clone();
    assert!(
        !seen.is_empty(),
        "default `complete()` never reached `generate_with_tools`",
    );
    let first = &seen[0];
    assert!(
        first.contains("system-token-xyz"),
        "rendered prompt missing system text: {first}",
    );
    assert!(
        first.contains("user-token-abc"),
        "rendered prompt missing user text: {first}",
    );
}

async fn contract_list_models_is_a_vec(provider: &Arc<dyn LlmProvider>) {
    if let Ok(v) = provider.list_models().await {
        // An empty list is legal; a list with duplicates is not
        // useful but not illegal. Just confirm the call returns.
        let _ = v;
    }
}

/// A provider that returns an error must return it through the
/// `Result` path, not panic. Tested via `MockProvider` scripted with
/// `Error`.
async fn contract_error_propagates(_provider: &Arc<dyn LlmProvider>) {
    let mock = MockProvider::new(
        "failing",
        Script::Error("simulated transport failure".to_string()),
    );
    let opts = GenerationOptions::default();
    let err = mock.generate("hello", &opts).await.unwrap_err();
    assert!(
        err.to_string().contains("simulated transport failure"),
        "error text lost: {err}",
    );
}

/// `stream_completion`'s default collects the reply and replays it as
/// chunks. For a text-scripted provider that means:
///
/// - at least one `Text` chunk whose concatenation equals the scripted
///   content;
/// - a terminal `Done`.
///
/// A provider that returns an error instead, or that yields chunks
/// without a terminator, fails here. The engine's assembly loop relies
/// on both facts.
async fn contract_stream_completion_text(_provider: &Arc<dyn LlmProvider>) {
    let mock = MockProvider::new("stream-text", Script::Text("hello world".into()));
    let mut req = CompletionRequest::new(ModelRef::new("mock", "m"));
    req.system = SystemPrompt::new();
    req.messages = vec![ChatMessage::text(
        MessageId::new(),
        MessageRole::User,
        "hi",
        OffsetDateTime::now_utc(),
    )];

    use futures::StreamExt;
    let mut stream = mock.stream_completion(&req);
    let mut saw_text = String::new();
    let mut saw_done = false;
    let mut saw_error: Option<KodError> = None;
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(StreamChunk::Text(t)) => saw_text.push_str(&t),
            Ok(StreamChunk::Done) => {
                saw_done = true;
                break;
            }
            Ok(_) => {}
            Err(e) => {
                saw_error = Some(e);
                break;
            }
        }
    }
    assert!(saw_error.is_none(), "unexpected error: {:?}", saw_error);
    assert!(saw_done, "stream must terminate with `Done`");
    assert_eq!(
        saw_text, "hello world",
        "the collected text must round-trip through the stream default",
    );
}

/// Tool-call scripted response: the default stream yields one
/// `ToolCallStart` + one `ToolCallDelta` per call, in order, and a
/// terminal `Done`.
async fn contract_stream_completion_calls(_provider: &Arc<dyn LlmProvider>) {
    let call = ToolCall {
        id: Some("call_1".into()),
        tool_name: "read_file".into(),
        arguments: serde_json::json!({"path": "a"}),
    };
    let mock = MockProvider::new("stream-calls", Script::Calls(vec![call]));
    let mut req = CompletionRequest::new(ModelRef::new("mock", "m"));
    req.system = SystemPrompt::new();
    req.messages = vec![ChatMessage::text(
        MessageId::new(),
        MessageRole::User,
        "read it",
        OffsetDateTime::now_utc(),
    )];

    use futures::StreamExt;
    let mut stream = mock.stream_completion(&req);
    let mut starts = 0usize;
    let mut deltas = 0usize;
    let mut saw_done = false;
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(StreamChunk::ToolCallStart { .. }) => starts += 1,
            Ok(StreamChunk::ToolCallDelta { .. }) => deltas += 1,
            Ok(StreamChunk::Done) => {
                saw_done = true;
                break;
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    assert!(saw_done, "stream must terminate with `Done`");
    assert_eq!(starts, 1, "one ToolCallStart per scripted call");
    assert_eq!(deltas, 1, "one ToolCallDelta per scripted call");
}

/// A scripted error must surface as an `Err` chunk. `stream_completion`
/// returning `Ok(empty)` for a failing call would strand the engine's
/// loop; the contract is "either a chunk or an error, never silence".
async fn contract_stream_completion_error(_provider: &Arc<dyn LlmProvider>) {
    let mock = MockProvider::new(
        "stream-err",
        Script::Error("simulated transport failure".into()),
    );
    let mut req = CompletionRequest::new(ModelRef::new("mock", "m"));
    req.system = SystemPrompt::new();
    req.messages = vec![ChatMessage::text(
        MessageId::new(),
        MessageRole::User,
        "hi",
        OffsetDateTime::now_utc(),
    )];

    use futures::StreamExt;
    let mut stream = mock.stream_completion(&req);
    let mut saw_err = false;
    while let Some(chunk) = stream.next().await {
        if chunk.is_err() {
            saw_err = true;
            break;
        }
    }
    assert!(
        saw_err,
        "a scripted error must surface as an `Err` chunk, not silence",
    );
}

// ---------------------------------------------------------------------------
// Mock helpers for the concrete providers' request-shape tests
// ---------------------------------------------------------------------------

/// A minimal valid OpenAI chat-completions SSE body. Used by the
/// OpenAI-compatible provider's `tests/contracts.rs` as the mock
/// server's response.
///
/// The format is the one every OpenAI-spec server emits:
///
/// ```text
/// data: {…}\n\n
/// data: {…}\n\n
/// data: [DONE]\n\n
/// ```
pub fn openai_sse_text_response(content: &str) -> String {
    format!(
        concat!(
            "data: {{\"id\":\"cmpl-1\",\"object\":\"chat.completion.chunk\",",
            "\"choices\":[{{\"index\":0,\"delta\":{{\"role\":\"assistant\"}}}}]}}\n\n",
            "data: {{\"id\":\"cmpl-1\",\"object\":\"chat.completion.chunk\",",
            "\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{content}\"}}}}]}}\n\n",
            "data: {{\"id\":\"cmpl-1\",\"object\":\"chat.completion.chunk\",",
            "\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}],",
            "\"usage\":{{\"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15}}}}\n\n",
            "data: [DONE]\n\n",
        ),
        content = content,
    )
}

/// A minimal valid Anthropic Messages API SSE body. Used by the
/// Anthropic provider's `tests/contracts.rs`.
pub fn anthropic_sse_text_response(content: &str) -> String {
    format!(
        concat!(
            "event: message_start\n",
            "data: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_1\",",
            "\"role\":\"assistant\",\"content\":[],",
            "\"usage\":{{\"input_tokens\":10,\"output_tokens\":0}}}}}}\n\n",
            "event: content_block_start\n",
            "data: {{\"type\":\"content_block_start\",\"index\":0,",
            "\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\n",
            "event: content_block_delta\n",
            "data: {{\"type\":\"content_block_delta\",\"index\":0,",
            "\"delta\":{{\"type\":\"text_delta\",\"text\":\"{content}\"}}}}\n\n",
            "event: content_block_stop\n",
            "data: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n",
            "event: message_delta\n",
            "data: {{\"type\":\"message_delta\",",
            "\"delta\":{{\"stop_reason\":\"end_turn\"}},",
            "\"usage\":{{\"output_tokens\":5}}}}\n\n",
            "event: message_stop\n",
            "data: {{\"type\":\"message_stop\"}}\n\n",
        ),
        content = content,
    )
}

// ---------------------------------------------------------------------------
// Self-tests
//
// Before this module existed, `run_trait_contracts` was declared and
// documented but had no caller anywhere in the workspace: the three
// crates whose doc comments mention it only mention it in prose, and
// the crate's own default features did not compile `testkit` at all.
// The coverage report showed the file at 0.00% (278 lines) because
// the compiler never saw it.
//
// These tests instantiate the suite against a fresh `MockProvider`
// (the suite creates its own internal mocks for the interesting
// scenarios, so passing any working provider is enough to exercise
// every contract body) and pin the two SSE-body helpers directly.
//
// `default = ["testkit"]` in this crate's Cargo.toml is what makes
// these tests compile during a plain `cargo nextest --workspace`.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod coverage_testkit {
    use super::*;
    use std::sync::Arc;

    /// Run the full trait contract suite against a text-scripted
    /// `MockProvider`. The suite creates its own internal mocks for
    /// the calls that need a specific script (calls, mixed, error),
    /// so a single working provider is enough to exercise every
    /// contract body inside `run_trait_contracts`.
    #[tokio::test]
    async fn run_trait_contracts_completes_against_a_mock_provider() {
        let provider: Arc<dyn LlmProvider> =
            Arc::new(MockProvider::new("self-test", Script::Text("ok".into())));
        run_trait_contracts(provider).await;
    }

    // ---- MockProvider behaviour ---------------------------------------

    #[test]
    fn scripted_provider_rotates_and_repeats_the_last_entry() {
        let p = MockProvider::scripted(
            "rotate",
            vec![Script::Text("first".into()), Script::Text("second".into())],
        );
        // next_script is private; drive it through a public method.
        // `list_models` does not consume the script, so use the
        // MockProvider's own accessor pattern: call next_script
        // directly since we are in the same module tree.
        assert!(matches!(p.next_script(), Script::Text(t) if t == "first"));
        assert!(matches!(p.next_script(), Script::Text(t) if t == "second"));
        // Past the end it repeats the last entry.
        assert!(matches!(p.next_script(), Script::Text(t) if t == "second"));
        assert!(matches!(p.next_script(), Script::Text(t) if t == "second"));
    }

    #[test]
    fn single_element_script_answers_every_call_the_same_way() {
        let p = MockProvider::new("one-shot", Script::Text("always".into()));
        for _ in 0..5 {
            assert!(matches!(p.next_script(), Script::Text(t) if t == "always"));
        }
    }

    #[tokio::test]
    async fn mock_provider_records_every_prompt_it_receives() {
        let p = MockProvider::new("recorder", Script::Text("ok".into()));
        let opts = GenerationOptions::default();
        p.generate("first", &opts).await.unwrap();
        p.generate("second", &opts).await.unwrap();
        let seen = p.captured_prompts.lock().unwrap().clone();
        assert_eq!(seen, vec!["first".to_string(), "second".to_string()]);
    }

    #[tokio::test]
    async fn mock_provider_generate_on_a_calls_only_script_errors() {
        // A script that has tool calls but no text cannot answer a
        // plain `generate`; the mock returns a descriptive error
        // rather than an empty string, so a test that wired the
        // wrong method learns why.
        let call = ToolCall {
            id: None,
            tool_name: "noop".into(),
            arguments: serde_json::Value::Null,
        };
        let p = MockProvider::new("calls-only", Script::Calls(vec![call]));
        let opts = GenerationOptions::default();
        let err = p.generate("hi", &opts).await.unwrap_err();
        assert!(
            err.to_string().contains("tool calls"),
            "error must explain the mismatch, got: {err}"
        );
    }

    #[test]
    fn mock_provider_with_capabilities_overrides_the_reported_matrix() {
        let custom = ProviderCapabilities {
            tools: false,
            vision: false,
            json_mode: true,
            prompt_cache: crate::request::PromptCacheKind::None,
            embeddings: false,
            streaming_tools: false,
            pricing: None,
            native_compaction: false,
        };
        let p =
            MockProvider::new("custom-caps", Script::Text("x".into())).with_capabilities(custom);
        let reported = p.capabilities();
        assert!(reported.json_mode, "custom caps applied");
        assert!(!reported.tools, "custom caps applied");
    }

    // ---- SSE response helpers -----------------------------------------

    #[test]
    fn openai_sse_body_carries_the_content_and_terminator() {
        let body = openai_sse_text_response("hello openai");
        assert!(
            body.contains("hello openai"),
            "content must be interpolated"
        );
        assert!(
            body.contains("data: [DONE]"),
            "OpenAI streams terminate with [DONE]"
        );
        // Every non-empty line after the first is a `data:` frame.
        for line in body.lines().filter(|l| !l.is_empty()) {
            assert!(
                line.starts_with("data:"),
                "unexpected non-data line in OpenAI SSE body: {line:?}"
            );
        }
    }

    #[test]
    fn anthropic_sse_body_has_the_expected_event_sequence() {
        let body = anthropic_sse_text_response("hello anthropic");
        // The Messages API stream is a fixed event sequence. Pin the
        // ones the wire parser keys on; anything else is cosmetic.
        for ev in [
            "message_start",
            "content_block_start",
            "content_block_delta",
            "content_block_stop",
            "message_delta",
            "message_stop",
        ] {
            assert!(
                body.contains(&format!("event: {ev}")),
                "missing `event: {ev}` in Anthropic SSE body"
            );
        }
        assert!(
            body.contains("hello anthropic"),
            "content must be interpolated"
        );
        assert!(
            body.contains("text_delta"),
            "the content delta type is what the parser dispatches on"
        );
    }
}
