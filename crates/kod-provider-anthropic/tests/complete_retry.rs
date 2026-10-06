//! F2i-8 regression: `complete()` retries transient HTTP errors.
//!
//! Pre-fix the non-streaming path wrapped only `.send()` in
//! `with_retry`, so a 503 (a *successful* HTTP exchange carrying an
//! error status) was never retried — the call failed on the first
//! transient blip. The fix maps a non-2xx status to a typed
//! `KodError` *inside* the retry closure.

use httpmock::prelude::*;
use kod_provider::request::{CompletionRequest, ModelRef, SystemPrompt};
use kod_provider::traits::{GenerationOptions, LlmProvider};
use kod_provider_anthropic::AnthropicProvider;
use kod_types::{ChatMessage, MessageId, MessageRole};
use time::OffsetDateTime;

fn user(text: &str) -> ChatMessage {
    ChatMessage::text(
        MessageId::new(),
        MessageRole::User,
        text,
        OffsetDateTime::now_utc(),
    )
}

fn provider_for(mock: &MockServer) -> AnthropicProvider {
    AnthropicProvider::with_api_key(mock.base_url(), "claude-sonnet-4-5", "test-key")
        .expect("provider construction")
}

fn request_with(messages: Vec<ChatMessage>) -> CompletionRequest {
    let mut req = CompletionRequest::new(ModelRef::new("test", "claude-sonnet-4-5"));
    req.system = SystemPrompt::default();
    req.messages = messages;
    req.options = GenerationOptions {
        max_tokens: Some(256),
        temperature: Some(0.3),
        ..Default::default()
    };
    req
}

#[tokio::test]
async fn complete_surfaces_a_permanent_503_after_retries() {
    // A server that only ever returns 503: `complete()` must retry
    // (per the policy) and then surface the typed error, not hang or
    // return Ok. Pre-fix, the status check sat outside `with_retry`,
    // so this returned the error after exactly ONE request; the fix
    // makes the retry loop actually run. Either way it must Err.
    let mock = MockServer::start_async().await;
    let endpoint = mock
        .mock_async(|when, then| {
            when.method(POST).path("/v1/messages");
            then.status(503)
                .header("content-type", "application/json")
                .body(r#"{"type":"error","error":{"type":"overloaded_error","message":"server busy"}}"#);
        })
        .await;
    let provider = provider_for(&mock);
    let req = request_with(vec![user("hello")]);
    let err = provider.complete(&req).await.expect_err("503 must surface");
    let msg = format!("{err}");
    assert!(!msg.is_empty(), "error must carry a message");
    // The fix routes the 503 through `with_retry`, so the endpoint is
    // hit more than once (default policy: 3 attempts).
    let hits = endpoint.hits_async().await;
    assert!(
        hits >= 2,
        "a transient 503 must be retried (>=2 requests), got {hits}"
    );
}

#[tokio::test]
async fn complete_does_not_retry_a_hard_error() {
    let mock = MockServer::start_async().await;
    let endpoint = mock
        .mock_async(|when, then| {
            when.method(POST).path("/v1/messages");
            then.status(401)
                .header("content-type", "application/json")
                .body(r#"{"type":"error","error":{"type":"authentication_error","message":"bad key"}}"#);
        })
        .await;

    let provider = provider_for(&mock);
    let req = request_with(vec![user("hello")]);
    let err = provider.complete(&req).await.expect_err("401 is not retryable");
    let _ = err;
    assert_eq!(
        endpoint.hits_async().await,
        1,
        "a 401 must not be retried — only one request"
    );
}
