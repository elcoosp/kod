//! §9.2 regression: the replay-safe stream retry contract.
//!
//! The attempt loop in `AnthropicProvider::stream_completion` decides
//! between *retrying* and *delivering* on three failure shapes:
//!
//! * a pre-commit HTTP error (5xx / 429 / network),
//! * a pre-commit stall (StreamGuard verdict),
//! * a clean-but-empty completion.
//!
//! The invariant that makes the loop replay-safe: a stream that has
//! already committed (emitted non-empty text or non-empty tool-call
//! arguments) must not be retried, because a retry would re-generate
//! content the caller already saw. These tests pin the fork.

use futures::StreamExt;
use httpmock::prelude::*;
use kod_provider::StreamChunk;
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

fn request_with(messages: Vec<ChatMessage>, system: SystemPrompt) -> CompletionRequest {
    let mut req = CompletionRequest::new(ModelRef::new("test", "claude-sonnet-4-5"));
    req.system = system;
    req.messages = messages;
    req.options = GenerationOptions {
        max_tokens: Some(256),
        temperature: Some(0.3),
        ..Default::default()
    };
    req
}

/// The minimal valid Anthropic SSE body the existing contracts test
/// uses: a `message_start` with no text, then `message_stop`. Yields
/// exactly one `Done` chunk and nothing else.
fn empty_sse_body() -> String {
    concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\",",
        "\"role\":\"assistant\",\"content\":[],",
        "\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    )
    .to_string()
}

/// An SSE body that delivers one text delta followed by `message_stop`.
fn text_sse_body(text: &str) -> String {
    format!(
        concat!(
            "event: message_start\n",
            "data: {{\"type\":\"message_start\",\"message\":{{\"id\":\"m1\",",
            "\"role\":\"assistant\",\"content\":[],",
            "\"usage\":{{\"input_tokens\":1,\"output_tokens\":0}}}}}}\n\n",
            "event: content_block_delta\n",
            "data: {{\"type\":\"content_block_delta\",\"index\":0,",
            "\"delta\":{{\"type\":\"text_delta\",\"text\":\"{}\"}}}}\n\n",
            "event: message_stop\n",
            "data: {{\"type\":\"message_stop\"}}\n\n",
        ),
        text,
    )
}

/// Drive the stream to completion. Collect every `Ok` chunk; stop and
/// return on the first `Err`.
async fn drain(
    provider: &AnthropicProvider,
    req: &CompletionRequest,
) -> (Vec<StreamChunk>, Option<kod_error::KodError>) {
    let mut stream = provider.stream_completion(req);
    let mut oks = Vec::new();
    let mut err = None;
    while let Some(item) = stream.next().await {
        match item {
            Ok(chunk) => oks.push(chunk),
            Err(e) => {
                err = Some(e);
                break;
            }
        }
    }
    (oks, err)
}

// ---------------------------------------------------------------------------
// 1. Pre-commit HTTP error retries up to the budget, then surfaces.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pre_commit_http_error_retries_up_to_budget_then_errors() {
    let mock = MockServer::start_async().await;
    let endpoint = mock
        .mock_async(|when, then| {
            when.method(POST).path("/v1/messages");
            // Every attempt gets a 503. The attempt loop's
            // `MAX_STREAM_ATTEMPTS = 3` budget means three POSTs.
            then.status(503).body("service unavailable");
        })
        .await;

    let provider = provider_for(&mock);
    let req = request_with(vec![user("hi")], SystemPrompt::default());
    let (oks, err) = drain(&provider, &req).await;

    assert!(err.is_some(), "expected an Err after the retry budget");
    assert!(
        oks.is_empty(),
        "no chunk should be delivered on a hard failure",
    );
    assert_eq!(
        endpoint.hits_async().await,
        3,
        "the attempt loop must issue exactly MAX_STREAM_ATTEMPTS requests",
    );
}

// ---------------------------------------------------------------------------
// 2. Clean-but-empty completion retries up to the budget, then delivers.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn empty_completion_retries_up_to_budget_then_delivers() {
    let mock = MockServer::start_async().await;
    let endpoint = mock
        .mock_async(|when, then| {
            when.method(POST).path("/v1/messages");
            // A clean 200 whose stream carries no text. Every
            // attempt is empty, so the EmptyCompletionRetry budget
            // drives the request count.
            then.status(200)
                .header("content-type", "text/event-stream")
                .body(empty_sse_body());
        })
        .await;

    let provider = provider_for(&mock);
    let req = request_with(vec![user("hi")], SystemPrompt::default());
    let (oks, err) = drain(&provider, &req).await;

    assert!(err.is_none(), "an empty completion is not a hard error");
    assert!(
        !oks.iter().any(|c| matches!(c, StreamChunk::Text(_))),
        "no text should be delivered from an empty stream",
    );
    assert!(
        oks.iter().any(|c| matches!(c, StreamChunk::Done)),
        "a Done must terminate the stream",
    );
    assert_eq!(
        endpoint.hits_async().await,
        3,
        "empty completions must be retried up to MAX_STREAM_ATTEMPTS",
    );
}

// ---------------------------------------------------------------------------
// 3. A committed stream is delivered on the first attempt (no retry).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn committed_stream_is_delivered_without_retry() {
    let mock = MockServer::start_async().await;
    let endpoint = mock
        .mock_async(|when, then| {
            when.method(POST).path("/v1/messages");
            then.status(200)
                .header("content-type", "text/event-stream")
                .body(text_sse_body("hi"));
        })
        .await;

    let provider = provider_for(&mock);
    let req = request_with(vec![user("hi")], SystemPrompt::default());
    let (oks, err) = drain(&provider, &req).await;

    assert!(err.is_none(), "a clean stream must not error");
    let texts: Vec<&str> = oks
        .iter()
        .filter_map(|c| match c {
            StreamChunk::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(texts, vec!["hi"], "the delivered text must be 'hi'");
    assert!(
        oks.iter().any(|c| matches!(c, StreamChunk::Done)),
        "a Done must terminate the stream",
    );
    assert_eq!(
        endpoint.hits_async().await,
        1,
        "a stream that committed must not be retried",
    );
}
