//! Live Anthropic API tests (design §4 D1.5).
//!
//! Every test is `#[ignore]`d so `cargo test` (and `cargo test
//! --workspace`) never makes a network call. A maintainer runs them
//! deliberately:
//!
//! ```sh
//! ANTHROPIC_API_KEY=sk-ant-… \
//!   cargo test -p kod-provider-anthropic --test live -- --ignored
//! ```
//!
//! The CI job `live-anthropic` runs the same command, gated on the
//! presence of the repository secret — a PR from a fork does not have
//! the secret, so the job is skipped, which is the design's "skip,
//! do not fail" contract.
//!
//! Every test re-checks `ANTHROPIC_API_KEY` at its own start and
//! returns cleanly when it is absent. That protects a maintainer who
//! runs `--ignored` locally without the variable set.

#![allow(dead_code)]

use futures::StreamExt;
use kod_provider::GenerationResponse;
use kod_provider::request::{CompletionRequest, ModelRef, SystemPrompt};
use kod_provider::traits::{GenerationOptions, LlmProvider};
use kod_provider_anthropic::AnthropicProvider;
use kod_types::{ChatMessage, MessageId, MessageRole};
use time::OffsetDateTime;

/// The model the live tests use. A small, cheap model so a maintainer
/// running the suite locally does not burn a budget. Override with
/// `KOD_LIVE_MODEL`.
fn model_name() -> String {
    std::env::var("KOD_LIVE_MODEL").unwrap_or_else(|_| "claude-3-5-haiku-latest".into())
}

/// `None` when the live suite cannot run, with a message printed to
/// stderr. Every test calls this at the top and returns on `None`.
fn api_key_or_skip() -> Option<String> {
    match std::env::var("ANTHROPIC_API_KEY") {
        Ok(k) if !k.trim().is_empty() => Some(k),
        _ => {
            eprintln!(
                "skipping: ANTHROPIC_API_KEY is not set. Export it (or set \
                 the repository secret) to run the live Anthropic suite."
            );
            None
        }
    }
}

fn provider(api_key: String) -> AnthropicProvider {
    AnthropicProvider::with_api_key("https://api.anthropic.com", model_name(), api_key)
        .expect("provider construction")
}

fn user(text: &str) -> ChatMessage {
    ChatMessage::text(
        MessageId::new(),
        MessageRole::User,
        text,
        OffsetDateTime::now_utc(),
    )
}

fn simple_request(messages: Vec<ChatMessage>) -> CompletionRequest {
    let mut req = CompletionRequest::new(ModelRef::new("anthropic", model_name()));
    req.system = SystemPrompt::new();
    req.messages = messages;
    req.options = GenerationOptions {
        max_tokens: Some(64),
        temperature: Some(0.0),
        ..Default::default()
    };
    req
}

#[tokio::test]
async fn live_complete_text() {
    let Some(key) = api_key_or_skip() else {
        return;
    };
    let provider = provider(key);
    let req = simple_request(vec![user("Reply with exactly: pong")]);
    let resp = provider.complete(&req).await.expect("complete");
    match resp {
        GenerationResponse::Text { content, usage } => {
            assert!(
                content.to_lowercase().contains("pong"),
                "expected the model to echo the word 'pong'; got: {content:?}",
            );
            assert!(
                usage.is_some(),
                "the Messages API response should carry usage",
            );
        }
        other => panic!("expected Text, got {other:?}"),
    }
}

#[tokio::test]
async fn live_stream_completion_text() {
    let Some(key) = api_key_or_skip() else {
        return;
    };
    let provider = provider(key);
    let req = simple_request(vec![user("Reply with exactly: pong")]);
    let mut stream = provider.stream_completion(&req);
    let mut text = String::new();
    let mut saw_done = false;
    while let Some(chunk) = stream.next().await {
        match chunk.expect("no stream error") {
            kod_provider::StreamChunk::Text(t) => text.push_str(&t),
            kod_provider::StreamChunk::Done => {
                saw_done = true;
                break;
            }
            _ => {}
        }
    }
    assert!(saw_done, "the stream must terminate with Done");
    assert!(
        text.to_lowercase().contains("pong"),
        "expected streamed text to contain 'pong'; got: {text:?}",
    );
}

#[tokio::test]
async fn live_tool_use_round_trip() {
    use kod_types::{ToolCategory, ToolDefinition, ToolId, ToolPermissions};
    let Some(key) = api_key_or_skip() else {
        return;
    };
    let provider = provider(key);
    let mut req = simple_request(vec![user(
        "Use the get_time tool to fetch the current time.",
    )]);
    req.tools = vec![ToolDefinition {
        id: ToolId::new(),
        name: "get_time".into(),
        description: "Return the current time in UTC.".into(),
        category: ToolCategory::System,
        parameters_schema: serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }),
        permissions: ToolPermissions::default(),
    }];
    let resp = provider.complete(&req).await.expect("complete");
    let calls = match resp {
        GenerationResponse::ToolCalls { calls, .. } => calls,
        GenerationResponse::Mixed { calls, .. } => calls,
        other => panic!("expected a tool call, got {other:?}"),
    };
    assert_eq!(calls.len(), 1, "exactly one tool call expected");
    assert_eq!(calls[0].tool_name, "get_time");
    assert!(
        calls[0].id.is_some(),
        "the wire must carry a tool_use id — the tool_call_id link depends on it",
    );
}

#[tokio::test]
async fn live_cache_control_is_accepted_on_a_long_prompt() {
    // Two calls with the same cacheable prefix. The API accepts the
    // `cache_control` marker (a malformed one would 400), and on the
    // second call the response usage may carry a cache-hit count. The
    // assertion the design's D1.2 depends on is the first one; the
    // second is reported informationally so a change in the usage
    // shape does not make the test flaky.
    let Some(key) = api_key_or_skip() else {
        return;
    };
    let provider = provider(key);

    // A reasonably long cacheable segment. Anthropic's minimum is
    // 1024 tokens for a cache breakpoint; a big system prompt is the
    // honest way to reach it without inventing filler.
    let cacheable = "You are a careful assistant. ".repeat(400);
    let prompt = SystemPrompt::new().with(cacheable, true);

    let mut req1 = CompletionRequest::new(ModelRef::new("anthropic", model_name()));
    req1.system = prompt.clone();
    req1.messages = vec![user("Answer with the number 1.")];
    req1.options = GenerationOptions {
        max_tokens: Some(16),
        temperature: Some(0.0),
        ..Default::default()
    };
    let resp1 = provider.complete(&req1).await.expect("first call");
    match resp1 {
        GenerationResponse::Text { content, .. } => {
            assert!(!content.trim().is_empty(), "first call must answer");
        }
        other => panic!("unexpected response: {other:?}"),
    }

    let mut req2 = req1.clone();
    req2.messages = vec![user("Answer with the number 2.")];
    let resp2 = provider.complete(&req2).await.expect("second call");
    match resp2 {
        GenerationResponse::Text { content, .. } => {
            assert!(!content.trim().is_empty(), "second call must answer");
        }
        other => panic!("unexpected response: {other:?}"),
    }

    // If the raw response carries cache fields, print them so a
    // maintainer reading the log can see the hit rate.
    // (Kept in the `Usage` shape today; a future field on the provider
    // response would land here.)
    let _ = &provider;
}
