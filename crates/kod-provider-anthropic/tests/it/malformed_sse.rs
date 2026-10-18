//! Malformed-SSE-frame resilience, end to end (session 18, fault-injection arc).
//!
//! Parity with `kod-provider-openai/tests/it/malformed_sse.rs`. The
//! wire-level unit test `sse_malformed_json_is_ignored` pins the
//! parser on a single frame in isolation; this test pins the behavior
//! the caller observes when one bad frame lands mid-stream in an
//! otherwise-valid body — the shape a truncated TCP write or a
//! frame-mangling proxy produces.
//!
//! Both adapters are documented to drop a malformed frame. If they
//! diverge (one drops, one errors-after-commit), the stream is already
//! committed on the erroring adapter (see `stream_retry.rs`), so the
//! retry loop does not recover it — the caller loses the rest of a
//! valid response. This test pins the drop-behavior.

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
    AnthropicProvider::with_api_key(mock.base_url(), "test-model", "test-key")
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

/// message_start, a text delta "before", a malformed frame, another
/// delta "after", message_stop. If the malformed frame is dropped, both
/// deltas survive and the stream ends cleanly. If it is fatal, "after"
/// and the stop reason are lost.
fn mixed_sse_body() -> String {
    concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\",",
        "\"role\":\"assistant\",\"content\":[],",
        "\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,",
        "\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,",
        "\"delta\":{\"type\":\"text_delta\",\"text\":\"before\"}}\n\n",
        "event: content_block_delta\n",
        "data: {this is not json\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,",
        "\"delta\":{\"type\":\"text_delta\",\"text\":\"after\"}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    )
    .to_string()
}

#[tokio::test]
async fn malformed_frame_mid_stream_does_not_lose_the_surrounding_text() {
    let mock = MockServer::start_async().await;
    let _endpoint = mock
        .mock_async(|when, then| {
            when.method(POST).path("/v1/messages");
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
        "message_stop after a malformed frame must still be observed",
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
