//! `LlmProvider` implementation over `adk_model::anthropic::Anthropic`.

use adk_core::{Content, GenerateContentConfig, Llm, LlmRequest, Part};
use adk_model::anthropic::{AnthropicClient, AnthropicConfig};
use async_trait::async_trait;
use futures::{Stream, StreamExt};
use kod_config::LlmConfig;
use kod_error::{KodError, Result};
use kod_provider::{
    GenerationOptions, GenerationResponse, LlmProvider, PromptCacheKind, ProviderCapabilities,
    StreamChunk,
};
use kod_types::{ToolCall, ToolDefinition};
use std::collections::HashMap;
use std::pin::Pin;

/// Fallback API key resolution: explicit argument first, then
/// `ANTHROPIC_API_KEY` from the environment. Unlike the local
/// OpenAI-compatible servers, Anthropic requires a real key — a
/// missing key is a startup error, not a fallback to a dummy value.
fn resolve_api_key(explicit: Option<String>) -> Result<String> {
    if let Some(k) = explicit
        && !k.trim().is_empty()
    {
        return Ok(k);
    }
    match std::env::var("ANTHROPIC_API_KEY") {
        Ok(k) if !k.trim().is_empty() => Ok(k),
        _ => Err(KodError::Config(
            "Anthropic provider needs an API key. Set \
             ANTHROPIC_API_KEY in the environment, or add \
             `api_key_env = \"ANTHROPIC_API_KEY\"` to the endpoint in the \
             config."
                .to_string(),
        )),
    }
}

/// Wrapper that implements kod's [`LlmProvider`] over the Anthropic
/// Messages API.
pub struct AnthropicProvider {
    inner: AnthropicClient,
    model: String,
    base_url: String,
    api_key: String,
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
        Ok(Self {
            inner,
            model,
            base_url,
            api_key,
        })
    }

    /// Create a provider, resolving the API key from the environment
    /// when `config.api_key` is absent.
    pub fn from_config(config: &LlmConfig, model_override: Option<&str>) -> Result<Self> {
        let model = model_override.unwrap_or(&config.model);
        let api_key = resolve_api_key(config.api_key.clone())?;
        Self::with_api_key(&config.base_url, model, api_key)
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
                        Part::FunctionCall { name, args, .. } => calls.push(ToolCall {
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
                                            Part::FunctionCall { name, args, .. } => {
                                                yield Ok(StreamChunk::ToolCallStart { name });
                                                yield Ok(StreamChunk::ToolCallDelta {
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
    fn resolve_api_key_prefers_explicit() {
        let k = resolve_api_key(Some("explicit".into())).unwrap();
        assert_eq!(k, "explicit");
    }

    #[test]
    fn resolve_api_key_errors_when_missing() {
        // SAFETY: single-threaded test.
        unsafe { std::env::remove_var("ANTHROPIC_API_KEY") };
        let err = resolve_api_key(None).unwrap_err();
        assert!(
            err.to_string().contains("ANTHROPIC_API_KEY"),
            "got: {err}"
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
