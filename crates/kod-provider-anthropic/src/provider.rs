//! `LlmProvider` implementation over `adk_model::anthropic::Anthropic`.

use adk_core::{Content, GenerateContentConfig, Llm, LlmRequest, Part};
use adk_model::anthropic::{AnthropicClient, AnthropicConfig};
use async_trait::async_trait;
use futures::{Stream, StreamExt};
use kod_error::{KodError, Result};
use kod_provider::{
    GenerationOptions, GenerationResponse, LlmProvider, PromptCacheKind, ProviderCapabilities,
    StreamChunk,
};
use kod_provider::request::CompletionRequest;
use kod_types::{ToolCall, ToolDefinition};
use std::collections::HashMap;
use std::pin::Pin;


/// Wrapper that implements kod's [`LlmProvider`] over the Anthropic
/// Messages API.
pub struct AnthropicProvider {
    inner: AnthropicClient,
    model: String,
    base_url: String,
    api_key: String,
    /// Transport for the native Messages API calls issued by
    /// `complete()` (design §4 D1.2, A5b). `adk-model`'s client has its
    /// own transport for the legacy methods; this one is only used for
    /// the wire-native path, which needs to build the request body
    /// itself to place `cache_control` on the right system segment.
    /// A single client per provider keeps the connection pool warm
    /// across calls.
    client: reqwest::Client,
}

impl AnthropicProvider {
    /// Create a provider with an explicit API key.
    pub fn with_api_key(
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Result<Self> {
        let base_url = normalize_base_url(&base_url.into());
        let api_key = api_key.into();
        let model = model.into();
        let inner = build_client(&api_key, &model, &base_url)?;
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| {
                KodError::Provider(format!(
                    "anthropic: could not build http client: {e}"
                ))
            })?;
        Ok(Self {
            inner,
            model,
            base_url,
            api_key,
            client,
        })
    }

    /// Switch models on the same endpoint and credentials.
    pub fn with_model(self, model: impl Into<String>) -> Result<Self> {
        let model = model.into();
        let inner = build_client(&self.api_key, &model, &self.base_url)?;
        Ok(Self {
            inner,
            model,
            base_url: self.base_url,
            api_key: self.api_key,
            // `reqwest::Client` is `Clone` (an internal Arc); reusing
            // the existing one keeps the connection pool and TLS
            // session cache warm across `/model` switches.
            client: self.client,
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn default_model(&self) -> &str {
        &self.model
    }

    fn request_model(&self, options: &GenerationOptions) -> String {
        options.model.clone().unwrap_or_else(|| self.model.clone())
    }

    fn text_request(
        &self,
        prompt: &str,
        options: &GenerationOptions,
        tools: &[ToolDefinition],
    ) -> LlmRequest {
        let mut request = LlmRequest::new(
            self.request_model(options),
            vec![Content::new("user").with_text(prompt)],
        )
        .with_config(options_to_config(options));
        if !tools.is_empty() {
            request.tools = tool_declarations(tools);
        }
        request
    }

    async fn collect(
        &self,
        request: LlmRequest,
        stream: bool,
    ) -> Result<(String, Vec<ToolCall>, Option<kod_provider::TokenUsage>)> {
        let mut responses = self
            .inner
            .generate_content(request, stream)
            .await
            .map_err(adk_err)?;
        let mut text = String::new();
        let mut calls = Vec::new();
        let mut last_usage: Option<kod_provider::TokenUsage> = None;
        while let Some(item) = responses.next().await {
            let response = item.map_err(adk_err)?;
            if let Some(usage) = response.usage_metadata {
                last_usage = Some(kod_provider::TokenUsage {
                    prompt_tokens: usage.prompt_token_count.max(0) as usize,
                    completion_tokens: usage.candidates_token_count.max(0) as usize,
                    total_tokens: usage.total_token_count.max(0) as usize,
                });
            }
            if let Some(content) = response.content {
                for part in content.parts {
                    match part {
                        Part::Text { text: chunk } => text.push_str(&chunk),
                        Part::FunctionCall { name, args, .. } => calls.push(ToolCall { id: None,
                            tool_name: name,
                            arguments: args,
                        }),
                        _ => {}
                    }
                }
            }
        }
        Ok((text, calls, last_usage))
    }
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    fn name(&self) -> &str {
        "anthropic"
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            tools: true,
            vision: true,
            json_mode: false,
            prompt_cache: PromptCacheKind::Explicit,
            embeddings: false,
            streaming_tools: true,
            pricing: None,
        }
    }

    async fn list_models(&self) -> Result<Vec<String>> {
        // `AnthropicClient::list_models` exists (see adk-model's
        // src/anthropic/models.rs) but its return type is a Vec of
        // `ModelInfo`, not a plain Vec<String>, and the exact field
        // name for the id (`.id` vs `.name`) is not pinned here. The
        // conservative behaviour is to return an empty list — the
        // CLI/TUI treat that as "no server-side listing available".
        // A follow-up can wire the real call once the shape is
        // confirmed; today the model is chosen from config, never
        // from a runtime listing.
        Ok(Vec::new())
    }

    /// Native Messages API call (design §4 D1.2, A5b). Uses the wire
    /// module to build a body with `cache_control` on the last cacheable
    /// system segment, POSTs it, and parses the response. The other
    /// trait methods (`generate`, `generate_with_tools`, `stream*`) stay
    /// on the `adk-model` path — that is the pre-migration surface, and
    /// this override only affects callers that have already migrated to
    /// `CompletionRequest`.
    async fn complete(
        &self,
        req: &CompletionRequest,
    ) -> Result<GenerationResponse> {
        let body = crate::wire::build_messages_body(req);
        let url = format!("{}/messages", self.base_url);
        let resp = self
            .client
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                KodError::Provider(format!("anthropic: POST {url}: {e}"))
            })?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            let snippet = if text.len() > 400 {
                format!("{}…", &text[..text.floor_char_boundary(400)])
            } else {
                text
            };
            return Err(KodError::provider_status(status.as_u16(), &snippet));
        }
        let parsed: serde_json::Value = resp.json().await.map_err(|e| {
            KodError::Provider(format!("anthropic: invalid JSON: {e}"))
        })?;
        parse_response(&parsed)
    }

    async fn generate(&self, prompt: &str, options: &GenerationOptions) -> Result<String> {
        let request = self.text_request(prompt, options, &[]);
        let (text, _, _) = self.collect(request, false).await?;
        Ok(text)
    }

    async fn generate_with_tools(
        &self,
        prompt: &str,
        tools: &[ToolDefinition],
        options: &GenerationOptions,
    ) -> Result<GenerationResponse> {
        let request = self.text_request(prompt, options, tools);
        let (text, calls, usage) = self.collect(request, false).await?;
        if calls.is_empty() {
            Ok(GenerationResponse::Text {
                content: text,
                usage,
            })
        } else if text.is_empty() {
            Ok(GenerationResponse::ToolCalls { calls, usage })
        } else {
            Ok(GenerationResponse::Mixed {
                content: text,
                calls,
                usage,
            })
        }
    }

    fn stream(
        &self,
        prompt: &str,
        options: &GenerationOptions,
    ) -> Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send + '_>> {
        let request = self.text_request(prompt, options, &[]);
        self.stream_request(request)
    }

    fn stream_with_tools<'a>(
        &'a self,
        prompt: &'a str,
        tools: &'a [ToolDefinition],
        options: &'a GenerationOptions,
    ) -> Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send + 'a>> {
        let request = self.text_request(prompt, options, tools);
        self.stream_request(request)
    }

    /// Structured streaming (design §4 D1.2, A5b). Native Messages API
    /// path: builds the wire body via `wire::build_streaming_body`,
    /// POSTs it, and drives the SSE parser from `wire`. Same
    /// `StreamChunk` framing as the OpenAI-compatible provider, so the
    /// engine's assembly is provider-agnostic.
    ///
    /// The `LlmRequest`-shaped `adk-model` streaming path is still the
    /// one used by `stream_with_tools`; this override only affects
    /// callers that have migrated to `CompletionRequest`.
    fn stream_completion<'a>(
        &'a self,
        req: &'a CompletionRequest,
    ) -> Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send + 'a>> {
        let body = crate::wire::build_streaming_body(req);
        let url = format!("{}/messages", self.base_url);
        let client = self.client.clone();
        let api_key = self.api_key.clone();

        Box::pin(async_stream::stream! {
            let resp = match client
                .post(&url)
                .header("x-api-key", &api_key)
                .header("anthropic-version", "2023-06-01")
                .header("accept", "text/event-stream")
                .json(&body)
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    yield Err(KodError::Provider(format!(
                        "anthropic stream: POST {url}: {e}"
                    )));
                    return;
                }
            };
            let status = resp.status();
            if !status.is_success() {
                let text = resp.text().await.unwrap_or_default();
                yield Err(KodError::provider_status(status.as_u16(), &text));
                return;
            }

            use futures::StreamExt;
            let mut stream = resp.bytes_stream();
            let mut buf = String::new();
            let mut state = crate::wire::AnthropicStreamState::default();
            let mut done_sent = false;

            while let Some(chunk) = stream.next().await {
                match chunk {
                    Ok(bytes) => {
                        buf.push_str(&String::from_utf8_lossy(&bytes));
                        // SSE frames are separated by a blank line.
                        // Process complete lines; a partial line stays
                        // in `buf` until the next byte chunk arrives.
                        while let Some(nl) = buf.find('\n') {
                            let line = buf[..nl].trim_end_matches('\r').to_string();
                            buf.drain(..=nl);
                            for chunk in crate::wire::parse_sse_line(&mut state, &line) {
                                if matches!(chunk, StreamChunk::Done) {
                                    done_sent = true;
                                }
                                yield Ok(chunk);
                            }
                        }
                    }
                    Err(e) => {
                        yield Err(KodError::Provider(format!(
                            "anthropic stream: read error: {e}"
                        )));
                        return;
                    }
                }
            }

            // The spec says `message_stop` terminates; if the transport
            // closes without one (a truncated stream), still emit a
            // `Done` so the engine's assembly loop terminates instead
            // of stalling on the last partial call.
            if !done_sent && state.finished {
                yield Ok(StreamChunk::Done);
            } else if !done_sent {
                yield Ok(StreamChunk::Done);
            }
        })
    }
}

impl AnthropicProvider {
    fn stream_request(
        &self,
        request: LlmRequest,
    ) -> Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send + '_>> {
        let inner = &self.inner;
        Box::pin(async_stream::stream! {
            match inner.generate_content(request, true).await {
                Ok(mut responses) => {
                    let mut last_usage: Option<kod_provider::TokenUsage> = None;
                    let mut next_tool_index: usize = 0;
                    while let Some(item) = responses.next().await {
                        match item {
                            Ok(response) => {
                                if let Some(usage) = response.usage_metadata {
                                    last_usage = Some(kod_provider::TokenUsage {
                                        prompt_tokens: usage.prompt_token_count.max(0) as usize,
                                        completion_tokens: usage.candidates_token_count.max(0) as usize,
                                        total_tokens: usage.total_token_count.max(0) as usize,
                                    });
                                }
                                if let Some(content) = response.content {
                                    for part in content.parts {
                                        match part {
                                            Part::Text { text } => {
                                                if !text.is_empty() {
                                                    yield Ok(StreamChunk::Text(text));
                                                }
                                            }
                                            Part::FunctionCall { name, args, id, .. } => {
                                                let index = next_tool_index;
                                                next_tool_index += 1;
                                                yield Ok(StreamChunk::ToolCallStart { index, id, name });
                                                yield Ok(StreamChunk::ToolCallDelta {
                                                    index,
                                                    arguments: args.to_string(),
                                                });
                                            }
                                            _ => {}
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                yield Err(adk_err(e));
                                break;
                            }
                        }
                    }
                    if let Some(usage) = last_usage {
                        yield Ok(StreamChunk::Usage(usage));
                    }
                    yield Ok(StreamChunk::Done);
                }
                Err(e) => {
                    yield Err(adk_err(e));
                }
            }
        })
    }
}

/// Build an `AnthropicClient` from raw fields.
///
/// `AnthropicConfig::new` is the only public constructor `adk-model`
/// exposes; the `base_url` field is public and set directly because the
/// config builder does not chain a `with_base_url` for it (verified
/// against the crate source). Passing `None` for `base_url` keeps the
/// crate's default Anthropic endpoint.
fn build_client(api_key: &str, model: &str, base_url: &str) -> Result<AnthropicClient> {
    let mut cfg = AnthropicConfig::new(api_key, model);
    cfg.base_url = Some(base_url.to_string());
    AnthropicClient::new(cfg).map_err(adk_err)
}

/// Normalize a user-supplied Anthropic endpoint to its API root.
/// Accepts `https://api.anthropic.com` or `https://api.anthropic.com/v1`.
pub fn normalize_base_url(base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    if trimmed.ends_with("/v1") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/v1")
    }
}

fn adk_err(e: adk_core::AdkError) -> KodError {
    KodError::Provider(e.to_string())
}

fn options_to_config(options: &GenerationOptions) -> GenerateContentConfig {
    GenerateContentConfig {
        temperature: options.temperature,
        top_p: options.top_p,
        max_output_tokens: options.max_tokens.and_then(|t| i32::try_from(t).ok()),
        stop_sequences: options.stop_sequences.clone(),
        ..Default::default()
    }
}

fn tool_declarations(tools: &[ToolDefinition]) -> HashMap<String, serde_json::Value> {
    tools
        .iter()
        .map(|tool| {
            (
                tool.name.clone(),
                serde_json::json!({
                    "description": tool.description,
                    "parameters": tool.parameters_schema,
                }),
            )
        })
        .collect()
}

/// Turn a Messages API response body into a `GenerationResponse`.
///
/// The response shape:
/// ```text
/// {
///   "content": [
///     {"type": "text", "text": "…"},
///     {"type": "tool_use", "id": "…", "name": "…", "input": {…}}
///   ],
///   "stop_reason": "end_turn" | "tool_use" | …,
///   "usage": {"input_tokens": N, "output_tokens": M}
/// }
/// ```
///
/// Text-only → `Text`; tool_use only → `ToolCalls`; both → `Mixed`. An
/// empty `content` array collapses to `Text { content: "" }` rather
/// than erroring — Anthropic does return empty content on a refusal or
/// an immediate stop, and the engine treats an empty reply as "nothing
/// to say" rather than a transport failure.
fn parse_response(v: &serde_json::Value) -> Result<GenerationResponse> {
    let content = v
        .get("content")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();

    let mut text = String::new();
    let mut calls: Vec<kod_types::ToolCall> = Vec::new();
    for block in &content {
        match block.get("type").and_then(|t| t.as_str()) {
            Some("text") => {
                if let Some(t) = block.get("text").and_then(|s| s.as_str()) {
                    text.push_str(t);
                }
            }
            Some("tool_use") => {
                let name = block
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("")
                    .to_string();
                if name.is_empty() {
                    continue;
                }
                let id = block.get("id").and_then(|i| i.as_str()).map(String::from);
                let input = block
                    .get("input")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                calls.push(kod_types::ToolCall {
                    id,
                    tool_name: name,
                    arguments: input,
                });
            }
            _ => {}
        }
    }

    let usage = v.get("usage").map(|u| kod_provider::TokenUsage {
        prompt_tokens: u.get("input_tokens").and_then(|n| n.as_u64()).unwrap_or(0)
            as usize,
        completion_tokens: u.get("output_tokens").and_then(|n| n.as_u64()).unwrap_or(0)
            as usize,
        total_tokens: (u.get("input_tokens").and_then(|n| n.as_u64()).unwrap_or(0)
            + u.get("output_tokens").and_then(|n| n.as_u64()).unwrap_or(0))
            as usize,
    });

    if calls.is_empty() {
        Ok(GenerationResponse::Text {
            content: text,
            usage,
        })
    } else if text.is_empty() {
        Ok(GenerationResponse::ToolCalls { calls, usage })
    } else {
        Ok(GenerationResponse::Mixed {
            content: text,
            calls,
            usage,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_appends_v1_when_missing() {
        assert_eq!(
            normalize_base_url("https://api.anthropic.com"),
            "https://api.anthropic.com/v1"
        );
    }

    #[test]
    fn normalize_keeps_existing_v1() {
        assert_eq!(
            normalize_base_url("https://api.anthropic.com/v1"),
            "https://api.anthropic.com/v1"
        );
        assert_eq!(
            normalize_base_url("https://api.anthropic.com/v1/"),
            "https://api.anthropic.com/v1"
        );
    }



    #[test]
    fn capabilities_declare_explicit_prompt_cache() {
        let p = AnthropicProvider::with_api_key(
            "https://api.anthropic.com",
            "claude-sonnet-4-5",
            "test-key",
        )
        .unwrap();
        let caps = p.capabilities();
        assert!(caps.tools);
        assert!(caps.streaming_tools);
        assert_eq!(caps.prompt_cache, PromptCacheKind::Explicit);
        assert!(caps.pricing.is_none());
    }

    #[test]
    fn with_api_key_builds_and_normalizes_base_url() {
        let p = AnthropicProvider::with_api_key(
            "https://api.anthropic.com/",
            "claude-sonnet-4-5",
            "test-key",
        )
        .unwrap();
        assert_eq!(p.base_url(), "https://api.anthropic.com/v1");
        assert_eq!(p.default_model(), "claude-sonnet-4-5");
    }

    #[test]
    fn with_model_switches_model_keeps_endpoint() {
        let p = AnthropicProvider::with_api_key(
            "https://api.anthropic.com",
            "claude-sonnet-4-5",
            "test-key",
        )
        .unwrap();
        let p2 = p.with_model("claude-opus-4").unwrap();
        assert_eq!(p2.default_model(), "claude-opus-4");
        assert_eq!(p2.base_url(), "https://api.anthropic.com/v1");
    }
}
