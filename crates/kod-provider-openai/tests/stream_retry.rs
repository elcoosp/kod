//! §9.2 regression: the replay-safe stream retry contract for the
//! OpenAI-compatible provider.
//!
//! Mirror of `kod-provider-anthropic/tests/stream_retry.rs` for the
//! chat-completions wire. Same three-way fork, same three tests:
//!
//! * a pre-commit HTTP error retries up to `MAX_STREAM_ATTEMPTS`
//!   before surfacing;
//! * a clean-but-empty completion retries via `EmptyCompletionRetry`;
//! * a stream that has already committed is delivered on the first
//!   attempt and never retried.
//!
//! The wire body is what `adk-model 2.2`'s `openai_compatible`
//! streaming parser consumes: newline-separated `data: <json>` lines
//! with `choices[0].delta.content`, terminated by a line that reads
//! exactly `data: [DONE]`. Empty lines between frames are skipped.
//!
//! Hit-count note: the `adk-model` inner transport wraps the initial
//! POST in its own `execute_with_retry`, so an all-503 test may see
//! more requests than our outer `MAX_STREAM_ATTEMPTS`. The
//! *committed* test is unaffected by the inner retry (its POST
//! succeeds on the first try), so it holds the strict `== 1` line.

use futures::StreamExt;
use httpmock::prelude::*;
use kod_provider::StreamChunk;
use kod_provider::request::{CompletionRequest, ModelRef, SystemPrompt};
use kod_provider::traits::{GenerationOptions, LlmProvider};
use kod_provider_openai::OpenAICompatProvider;
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

fn provider_for(mock: &MockServer) -> OpenAICompatProvider {
    OpenAICompatProvider::with_api_key(mock.base_url(), "test-model", "test-key")
        .expect("provider construction")
}

fn request_with(messages: Vec<ChatMessage>, system: SystemPrompt) -> CompletionRequest {
    let mut req = CompletionRequest::new(ModelRef::new("test", "test-model"));
    req.system = system;
    req.messages = messages;
    req.options = GenerationOptions {
        max_tokens: Some(256),
        temperature: Some(0.3),
        ..Default::default()
    };
    req
}

/// The minimal OpenAI-compatible SSE body: a single terminator frame.
/// Parses to zero `LlmResponse` chunks with text — i.e. a clean but
/// empty completion.
fn empty_sse_body() -> String {
    "data: [DONE]\n\n".to_string()
}

/// One text delta followed by the terminator.
fn text_sse_body(text: &str) -> String {
    format!(
        "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{}\"}}}}]}}\n\ndata: [DONE]\n\n",
        text,
    )
}

async fn drain(
    provider: &OpenAICompatProvider,
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
// 1. Pre-commit HTTP error retries up to (at least) the budget.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pre_commit_http_error_retries_up_to_budget_then_errors() {
    let mock = MockServer::start_async().await;
    let endpoint = mock
        .mock_async(|when, then| {
            when.method(POST).path("/v1/chat/completions");
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
    let hits = endpoint.hits_async().await;
    assert!(
        hits >= 3,
        "the outer attempt loop must issue at least MAX_STREAM_ATTEMPTS requests, got {hits}",
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
            when.method(POST).path("/v1/chat/completions");
            // A clean 200 whose stream carries no text. `adk-model`
            // does not retry on a successful response, so the outer
            // `EmptyCompletionRetry` budget drives the request count.
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
    let hits = endpoint.hits_async().await;
    assert_eq!(
        hits, 3,
        "empty completions must be retried up to MAX_STREAM_ATTEMPTS, got {hits}",
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
            when.method(POST).path("/v1/chat/completions");
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
