//! Provider request-shape contracts for the OpenAI-compatible provider
//! (design D1.5, §11.2).
//!
//! The trait-level suite (`kod_provider::testkit::run_trait_contracts`)
//! proves the `LlmProvider` surface behaves the same across
//! implementors. This file proves the OpenAI-compatible wire format is
//! the OpenAI spec — what the server on the other end of the socket
//! expects to receive.
//!
//! Each test starts an `httpmock` server that only answers a POST whose
//! body satisfies the shape the test is asserting. If the provider
//! emits the wrong body, the mock returns 404 and `complete()` fails,
//! which is the assertion. This is the shape that avoids depending on
//! httpmock's request-body inspection API (unstable across minor
//! versions) and instead uses the request *matcher* API, which is the
//! one httpmock has kept stable.

use httpmock::prelude::*;
use kod_provider::GenerationResponse;
use kod_provider::request::{CompletionRequest, ModelRef, SystemPrompt, SystemSegment};
use kod_provider::traits::{GenerationOptions, LlmProvider};
use kod_provider_openai::OpenAICompatProvider;
use kod_types::{ChatMessage, MessageId, MessageRole, ToolCall};
use serde_json::Value;
use time::OffsetDateTime;

fn user(text: &str) -> ChatMessage {
    ChatMessage::text(
        MessageId::new(),
        MessageRole::User,
        text,
        OffsetDateTime::now_utc(),
    )
}

fn assistant_with_call(text: &str, id: &str, name: &str, args: Value) -> ChatMessage {
    let mut m = ChatMessage::text(
        MessageId::new(),
        MessageRole::Assistant,
        text,
        OffsetDateTime::now_utc(),
    );
    m.tool_calls.push(ToolCall {
        id: Some(id.into()),
        tool_name: name.into(),
        arguments: args,
    });
    m
}

fn tool_result(id: &str, content: &str) -> ChatMessage {
    let mut m = ChatMessage::text(
        MessageId::new(),
        MessageRole::Tool,
        content,
        OffsetDateTime::now_utc(),
    );
    m.tool_call_id = Some(id.into());
    m
}

fn simple_text_response() -> String {
    r#"{
        "id": "chatcmpl-1",
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "ok"},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7}
    }"#
    .to_string()
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

#[tokio::test]
async fn complete_posts_to_chat_completions_with_system_message() {
    let mock = MockServer::start_async().await;
    let endpoint = mock
        .mock_async(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .body_contains("\"role\":\"system\"")
                .body_contains("identity line");
            then.status(200)
                .header("content-type", "application/json")
                .body(simple_text_response());
        })
        .await;

    let provider = provider_for(&mock);
    let req = request_with(
        vec![user("hi")],
        SystemPrompt {
            segments: vec![SystemSegment {
                text: "identity line".into(),
                cacheable: true,
            }],
        },
    );
    let resp = provider.complete(&req).await.expect("complete");
    match resp {
        GenerationResponse::Text { content, .. } => assert_eq!(content, "ok"),
        other => panic!("expected Text, got {other:?}"),
    }
    assert_eq!(endpoint.hits_async().await, 1);
}

#[tokio::test]
async fn complete_concatenates_cacheable_and_volatile_segments() {
    let mock = MockServer::start_async().await;
    // Both segments must appear in the request body. The mock only
    // answers if both substrings are present.
    let _endpoint = mock
        .mock_async(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .body_contains("identity line")
                .body_contains("volatile env");
            then.status(200)
                .header("content-type", "application/json")
                .body(simple_text_response());
        })
        .await;

    let provider = provider_for(&mock);
    let req = request_with(
        vec![user("hi")],
        SystemPrompt {
            segments: vec![
                SystemSegment {
                    text: "identity line".into(),
                    cacheable: true,
                },
                SystemSegment {
                    text: "volatile env".into(),
                    cacheable: false,
                },
            ],
        },
    );
    provider.complete(&req).await.expect("complete");
}

#[tokio::test]
async fn complete_preserves_tool_call_id_and_tool_result_link() {
    let mock = MockServer::start_async().await;
    // The wire body must carry the assistant's tool call with id
    // `call_42` and the tool result linked by the same id. httpmock
    // matches on substrings so we assert both halves.
    let _endpoint = mock
        .mock_async(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .body_contains("call_42")
                .body_contains("read_file")
                .body_contains("fn main");
            then.status(200)
                .header("content-type", "application/json")
                .body(simple_text_response());
        })
        .await;

    let provider = provider_for(&mock);
    let req = request_with(
        vec![
            user("read the file"),
            assistant_with_call(
                "",
                "call_42",
                "read_file",
                serde_json::json!({"path": "src/main.rs"}),
            ),
            tool_result("call_42", "fn main() {}"),
        ],
        SystemPrompt::default(),
    );
    provider.complete(&req).await.expect("complete");
}

#[tokio::test]
async fn complete_serializes_tools_as_functions() {
    use kod_types::{ToolCategory, ToolDefinition, ToolId, ToolPermissions};
    let mock = MockServer::start_async().await;
    let _endpoint = mock
        .mock_async(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                .body_contains("\"read_file\"")
                .body_contains("\"parameters\"");
            then.status(200)
                .header("content-type", "application/json")
                .body(simple_text_response());
        })
        .await;

    let provider = provider_for(&mock);
    let mut req = request_with(vec![user("read a file")], SystemPrompt::default());
    req.tools = vec![ToolDefinition {
        trust_level: kod_types::trust::TrustLevel::default(),
        id: ToolId::new(),
        name: "read_file".into(),
        description: "read a file".into(),
        category: ToolCategory::FileSystem,
        parameters_schema: serde_json::json!({
            "type": "object",
            "properties": {"path": {"type": "string"}}
        }),
        permissions: ToolPermissions::default(),
    }];
    provider.complete(&req).await.expect("complete");
}

#[tokio::test]
async fn stream_completion_posts_to_chat_completions() {
    let mock = MockServer::start_async().await;
    let _endpoint = mock
        .mock_async(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200)
                .header("content-type", "text/event-stream")
                .body("data: [DONE]\n\n");
        })
        .await;

    let provider = provider_for(&mock);
    let req = request_with(vec![user("hi")], SystemPrompt::default());
    use futures::StreamExt;
    let mut stream = provider.stream_completion(&req);
    // Drain the stream; the request has been made by the time the
    // first poll returns.
    while let Some(_chunk) = stream.next().await {}
}
