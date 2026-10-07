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
//! Plus the tab-bridge rate-limit pair: a 429 whose body embeds the
//! window text is slept out exactly once (within the configured
//! `with_rate_limit_wait` budget) and re-driven; without a budget the
//! same 429 surfaces after the attempt budget, as before.
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
// 3. tab-bridge rate-limit contract: wait out the window, then re-drive.
// ---------------------------------------------------------------------------

/// The real tab-bridge 429 shape (`src/facade/errors.ts`), with the window
/// text shortened to a 1 s hint so the test sleeps a second, not twenty
/// minutes.
fn tab_bridge_429_body(hint: &str) -> String {
    format!(
        r#"{{"error":{{"message":"provider reports rate limiting (Messages too frequent); {hint}","type":"tab_bridge_error","code":"rate_limited"}}}}"#
    )
}

/// tab-bridge contract: 429 whose body embeds the window text, then the
/// real answer once the window "elapses". With a wait budget >= the hint,
/// the provider must sleep out the hint (shortened here to 1 s) and
/// re-drive the SAME request instead of surfacing the error.
#[tokio::test]
async fn rate_limited_request_waits_out_hint_then_succeeds() {
    let mock = MockServer::start_async().await;
    // First-registered mock wins httpmock's matcher, so the 429 answers
    // until the watcher below deletes it, then the success mock takes over.
    let limited = mock
        .mock_async(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(429)
                .header("content-type", "application/json")
                .body(tab_bridge_429_body("retry after 1 second"));
        })
        .await;
    let ok_endpoint = mock
        .mock_async(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200)
                .header("content-type", "text/event-stream")
                .body(text_sse_body("hi"));
        })
        .await;

    let provider = provider_for(&mock).with_rate_limit_wait(std::time::Duration::from_secs(5));
    let req = request_with(vec![user("hi")], SystemPrompt::default());

    // Lift the 429 mock as soon as it has answered once, concurrently with
    // the request under test: the provider's hint sleep gives the deletion
    // ~1 s of real time to land before the retry arrives.
    let drain_fut = async { drain(&provider, &req).await };
    let lift_fut = async {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut hits = 0usize;
        while hits == 0 && std::time::Instant::now() < deadline {
            hits = limited.hits_async().await;
            if hits == 0 {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }
        if hits > 0 {
            limited.delete_async().await;
        }
        hits
    };
    let ((oks, err), limited_hits) = futures::future::join(drain_fut, lift_fut).await;

    assert!(
        limited_hits >= 1,
        "the 429 mock must answer at least once, got {limited_hits} hits"
    );
    assert!(
        err.is_none(),
        "a waited-out 429 must re-drive, not fail: {err:?}"
    );
    let texts: Vec<&str> = oks
        .iter()
        .filter_map(|c| match c {
            StreamChunk::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        texts,
        vec!["hi"],
        "the retried request must deliver the text"
    );
    assert!(
        ok_endpoint.hits_async().await >= 1,
        "the success mock must have served the retry"
    );
}

/// Without a budget (default), a 429 must NOT be slept out: it surfaces
/// after the attempt budget exactly like before (behavior lock).
#[tokio::test]
async fn rate_limit_without_wait_budget_fails_fast_as_before() {
    let mock = MockServer::start_async().await;
    let endpoint = mock
        .mock_async(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(429)
                .header("content-type", "application/json")
                .body(tab_bridge_429_body("wait ~20 minutes before retrying"));
        })
        .await;

    let provider = provider_for(&mock); // no with_rate_limit_wait
    let req = request_with(vec![user("hi")], SystemPrompt::default());
    let started = std::time::Instant::now();
    let (oks, err) = drain(&provider, &req).await;

    let err = err.expect("a 429 with no wait budget must surface an error");
    let msg = err.to_string().to_ascii_lowercase();
    assert!(
        msg.contains("429") || msg.contains("rate"),
        "error should carry the rate-limit signal, got: {msg}"
    );
    assert!(
        oks.is_empty(),
        "no chunk should be delivered on a hard failure"
    );
    assert!(
        endpoint.hits_async().await >= 3,
        "the attempt budget must be exhausted, got {} hits",
        endpoint.hits_async().await
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "without a budget the window must not be slept out (took {:?})",
        started.elapsed()
    );
}

// ---------------------------------------------------------------------------
// 4. A committed stream is delivered on the first attempt (no retry).
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
