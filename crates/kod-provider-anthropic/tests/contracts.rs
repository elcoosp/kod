//! Provider contract tests for `AnthropicProvider` (§11.2).
//!
//! Same two layers as the OpenAI provider's suite. The Anthropic
//! request shape is `/v1/messages` with a top-level `system` field
//! and structured `messages` — the tests assert that the call
//! reaches the mock, not the exact JSON the wire parser produces
//! (that is `adk-model`'s contract).

use httpmock::prelude::*;
use kod_provider::request::{CompletionRequest, ModelRef, SystemPrompt};
use kod_provider::testkit::{
    MockProvider, Script, anthropic_sse_text_response, run_trait_contracts,
};
use kod_provider::traits::{GenerationOptions, LlmProvider};
use kod_provider_anthropic::AnthropicProvider;
use kod_types::{ChatMessage, MessageId, MessageRole};
use std::sync::Arc;
use time::OffsetDateTime;

#[tokio::test]
async fn anthropic_provider_passes_trait_contracts() {
    let mock = Arc::new(MockProvider::new(
        "anthropic-mock",
        Script::Text("contract".to_string()),
    ));
    run_trait_contracts(mock).await;
}

#[tokio::test]
async fn anthropic_provider_sends_structured_messages() {
    let server = MockServer::start();
    let endpoint = server.mock(|when, then| {
        when.method(POST).path("/v1/messages");
        then.status(200)
            .header("content-type", "text/event-stream")
            .body(anthropic_sse_text_response("pong"));
    });

    let provider = AnthropicProvider::with_api_key(
        server.base_url(),
        "claude-test",
        "test-key",
    )
    .expect("build provider");

    let mut req = CompletionRequest::new(ModelRef::new("anthropic", "claude-test"));
    req.system = SystemPrompt::new().with("You are a test.", true);
    req.messages = vec![ChatMessage::text(
        MessageId::new(),
        MessageRole::User,
        "ping",
        OffsetDateTime::now_utc(),
    )];

    let _ = provider.complete(&req).await;
    endpoint.assert();
}

#[tokio::test]
async fn anthropic_provider_streams_sse_text() {
    let server = MockServer::start();
    let _mock = server.mock(|when, then| {
        when.method(POST).path("/v1/messages");
        then.status(200)
            .header("content-type", "text/event-stream")
            .body(anthropic_sse_text_response("hi"));
    });

    let provider = AnthropicProvider::with_api_key(
        server.base_url(),
        "claude-test",
        "test-key",
    )
    .expect("build provider");

    use futures::StreamExt;
    let opts = GenerationOptions::default();
    let tools: Vec<kod_types::ToolDefinition> = Vec::new();
    let mut stream = provider.stream_with_tools("ping", &tools, &opts);
    while let Some(_item) = stream.next().await {}
}
