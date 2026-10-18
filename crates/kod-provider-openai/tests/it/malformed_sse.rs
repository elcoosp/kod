//! Malformed-SSE-frame resilience, end to end (session 18, fault-injection arc).
//!
//! `wire.rs` unit tests pin the parser on individual frames.
//! `stream_retry.rs` pins retry on whole-stream failures. Neither pins
//! the shape a truncated TCP write, a frame-mangling proxy, or a junk
//! keepalive produces: one bad frame in the middle of an otherwise-
//! valid SSE body.
//!
//! The anthropic adapter names the intended behavior in
//! `sse_malformed_json_is_ignored` ("dropped malformed JSON frame").
//! This test pins the same for the openai-compatible adapter at the
//! level the caller sees. If the adapter treats a single bad frame as
//! a stream error *after* text has been delivered, the stream is
//! already committed (see `stream_retry.rs`), so the retry loop will
//! not recover it — the caller loses the rest of a valid response to
//! one malformed line.

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

fn request() -> CompletionRequest {
    let mut req = CompletionRequest::new(ModelRef::new("test", "test-model"));
    req.system = SystemPrompt::default();
    req.messages = vec![user("hi")];
    req.options = GenerationOptions {
        max_tokens: Some(256),
        temperature: Some(0.3),
        ..Default::default()
    };
    req
}

/// A body with a good frame, a malformed frame, a good frame, and the
/// terminator. If the adapter treats the malformed frame as fatal,
/// the second good text and the terminator are lost. If it drops
/// the bad frame, both good texts survive and the stream completes.
fn mixed_sse_body() -> String {
    "data: {\"choices\":[{\"delta\":{\"content\":\"before\"}}]}\n\n\
     data: {this is not json\n\n\
     data: {\"choices\":[{\"delta\":{\"content\":\"after\"}}]}\n\n\
     data: [DONE]\n\n"
        .to_string()
}

#[tokio::test]
async fn malformed_frame_mid_stream_does_not_lose_the_surrounding_text() {
    let mock = MockServer::start_async().await;
    let _endpoint = mock
        .mock_async(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200)
                .header("content-type", "text/event-stream")
                .body(mixed_sse_body());
        })
        .await;

    let provider = provider_for(&mock);
    let req = request();
    let mut stream = provider.stream_completion(&req);
    let mut collected = String::new();
    let mut saw_done = false;
    let mut err = None;
    while let Some(item) = stream.next().await {
        match item {
            Ok(StreamChunk::Text(t)) => collected.push_str(&t),
            Ok(StreamChunk::Done) => {
                saw_done = true;
                break;
            }
            Ok(_) => {}
            Err(e) => {
                err = Some(e);
                break;
            }
        }
    }

    assert!(
        err.is_none(),
        "a malformed frame mid-stream must be dropped, not surfaced as a \
         stream error; got {err:?}",
    );
    assert!(
        saw_done,
        "the [DONE] terminator after a malformed frame must still be observed",
    );
    assert!(
        collected.contains("before"),
        "the good frame before the malformed one must survive; got {collected:?}",
    );
    assert!(
        collected.contains("after"),
        "the good frame after the malformed one must survive (a single bad \
         frame must not truncate the rest of the stream); got {collected:?}",
    );
}
