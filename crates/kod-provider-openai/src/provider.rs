//! `LlmProvider` implementation over `adk_model::openai_compatible::OpenAICompatible`.

use adk_core::{Content, GenerateContentConfig, Llm, LlmRequest, Part};
use adk_model::openai_compatible::{OpenAICompatible, OpenAICompatibleConfig};
use async_trait::async_trait;
use futures::{Stream, StreamExt};
use kod_config::LlmConfig;
use kod_error::{KodError, Result};
use kod_provider::{GenerationOptions, GenerationResponse, LlmProvider, StreamChunk};
use kod_types::{ToolCall, ToolDefinition};
use std::collections::HashMap;
use std::pin::Pin;

/// Fallback API key for local servers (Ollama, LM Studio, MLX) that accept any value.
const LOCAL_FALLBACK_API_KEY: &str = "not-needed";

/// Retries attempted on a transient provider error (rate limit, 5xx,
/// connection reset). Local model servers routinely cold-start on the
/// first request — the very first prompt against a freshly-started
/// Ollama frequently fails once and then succeeds on retry. Three
/// attempts (1 initial + 2 retries) with exponential backoff covers
/// that case without making a genuinely-broken endpoint feel like a
/// hang.
const MAX_RETRIES: u32 = 3;

/// Base backoff in milliseconds. Doubled each retry: 500, 1000.
/// Kept short because the user is staring at a live terminal — a
/// 30-second wait on the third try would be worse than the original
/// error.
const RETRY_BACKOFF_MS: u64 = 500;

/// Classify whether an error is worth retrying. Retrying a 401 just
/// wastes the user's time and hides the real problem.
fn is_retryable(e: &str) -> bool {
    let lower = e.to_lowercase();
    lower.contains("rate limit")
        || lower.contains("429")
        || lower.contains("too many requests")
        || lower.contains("timeout")
        || lower.contains("timed out")
        || lower.contains("connection reset")
        || lower.contains("connection closed")
        || lower.contains("temporarily")
        || lower.contains("try again")
        || lower.contains("503")
        || lower.contains("502")
        || lower.contains("504")
        || lower.contains("bad gateway")
        || lower.contains("service unavailable")
        || lower.contains("gateway timeout")
}

/// Adapter that implements kod's [`LlmProvider`] for any OpenAI-compatible endpoint.
pub struct OpenAICompatProvider {
    inner: OpenAICompatible,
    model: String,
    base_url: String,
    api_key: String,
    /// Shared HTTP client for the small set of requests kod issues
    /// directly (currently `GET /v1/models`). Built once per provider so
    /// the connection pool, TLS session cache, and background runtime
    /// are reused across calls. The previous code constructed a fresh
    /// `reqwest::Client` on every `list_models()` — each one spins up
    /// its own pool and a background task, all of which are dropped as
    /// soon as the response lands.
    ///
    /// The client is tied to `base_url` + `api_key`, not to the model,
    /// so [`OpenAICompatProvider::with_model`] carries the existing
    /// client forward rather than building a new one. A `/model` switch
    /// in the TUI (or any other model change) now keeps the warm TCP
    /// and TLS state, which is what makes back-to-back switches cheap.
    client: reqwest::Client,
    timeout_secs: u64,
}

impl OpenAICompatProvider {
    /// Create a provider for `base_url` (server root or `.../v1`) with `model`.
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Result<Self> {
        Self::with_api_key(base_url, model, resolve_api_key(None))
    }

    /// Create a provider with an explicit API key.
    pub fn with_api_key(
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Result<Self> {
        Self::with_api_key_and_timeout(base_url, model, api_key, 300)
    }

    /// Like `with_api_key` but with a configurable request timeout.
    pub fn with_api_key_and_timeout(
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key: impl Into<String>,
        timeout_secs: u64,
    ) -> Result<Self> {
        let base_url = normalize_base_url(&base_url.into());
        let api_key = api_key.into();
        let model = model.into();
        let inner = OpenAICompatible::new(
            OpenAICompatibleConfig::new(&api_key, &model)
                .with_base_url(&base_url)
                .with_provider_name("openai-compatible"),
        )
        .map_err(adk_err)?;
        // One client per provider. A rustls-backed reqwest client
        // carries a connection pool and a TLS session cache that are
        // worth keeping warm; the pool is also what makes back-to-back
        // `/model` switches cheap.
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(timeout_secs))
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| KodError::Provider(format!("could not build http client: {e}")))?;
        Ok(Self {
            inner,
            model,
            base_url,
            api_key,
            client,
            timeout_secs,
        })
    }

    /// Build a provider from kod's LLM config, optionally overriding the model
    /// (e.g. from a `--model` CLI flag).
    pub fn from_config(config: &LlmConfig, model_override: Option<&str>) -> Result<Self> {
        let model = model_override.unwrap_or(&config.model);
        Self::with_api_key_and_timeout(
            &config.base_url,
            model,
            resolve_api_key(config.api_key.clone()),
            config.timeout_secs,
        )
    }

    /// Switch models, keeping the same endpoint, credentials, and
    /// HTTP client.
    ///
    /// The client is bound to `base_url` and `api_key` — which do not
    /// change on a model switch — so it is carried forward instead of
    /// rebuilt. A `with_api_key` call would construct a fresh
    /// `reqwest::Client` (connection pool + TLS session cache + a
    /// background runtime), throw away the warm one, and force the
    /// next request to establish new TCP and TLS state for no reason.
    /// The TUI's `/model` switch is the common caller; back-to-back
    /// switches are now cheap.
    pub fn with_model(self, model: impl Into<String>) -> Result<Self> {
        let model = model.into();
        // Build a new inner OpenAICompatible (that struct holds the
        // model), but reuse the fields that do not depend on it.
        let inner = OpenAICompatible::new(
            OpenAICompatibleConfig::new(&self.api_key, &model)
                .with_base_url(&self.base_url)
                .with_provider_name("openai-compatible"),
        )
        .map_err(adk_err)?;
        Ok(Self {
            inner,
            model,
            base_url: self.base_url,
            api_key: self.api_key,
            // Reuse the existing client. reqwest::Client is Clone
            // (it wraps an Arc internally), so this is an atomic
            // increment, not a rebuild.
            client: self.client,
            timeout_secs: self.timeout_secs,
        })
    }

    /// The normalized API root (`.../v1`).
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// The default model used when request options don't specify one.
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

    /// Run a request and split the collected stream into text + tool calls.
    ///
    /// Transient errors (rate limit, connection reset, 5xx) are retried
    /// with exponential backoff up to [`MAX_RETRIES`]. Non-retryable
    /// errors (auth, model not found, malformed request) return
    /// immediately — retrying them only wastes time.
    async fn collect(
        &self,
        request: LlmRequest,
        stream: bool,
    ) -> Result<(String, Vec<ToolCall>, Option<kod_provider::TokenUsage>)> {
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            match self.collect_once(&request, stream).await {
                Ok(v) => return Ok(v),
                Err(e) => {
                    let msg = e.to_string();
                    if attempt < MAX_RETRIES && is_retryable(&msg) {
                        let delay = RETRY_BACKOFF_MS * (1u64 << (attempt - 1).min(3));
                        tracing::warn!(
                            attempt,
                            max_attempts = MAX_RETRIES,
                            delay_ms = delay,
                            error = %msg,
                            "transient provider error; retrying"
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                        continue;
                    }
                    return Err(e);
                }
            }
        }
    }

    /// One attempt of the request. Split out so `collect` can retry
    /// without rebuilding anything.
    async fn collect_once(
        &self,
        request: &LlmRequest,
        stream: bool,
    ) -> Result<(String, Vec<ToolCall>, Option<kod_provider::TokenUsage>)> {
        let mut responses = self
            .inner
            .generate_content(request.clone(), stream)
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
impl LlmProvider for OpenAICompatProvider {
    fn name(&self) -> &str {
        "openai-compatible"
    }

    async fn list_models(&self) -> Result<Vec<String>> {
        let url = format!("{}/models", self.base_url);
        let response = self
            .client
            .get(&url)
            .bearer_auth(&self.api_key)
            .send()
            .await
            .map_err(|e| {
                // Include the URL. A misconfigured `base_url` (missing
                // `/v1`, wrong port, http vs https) is the most common
                // cause of this failure, and the raw transport error
                // — "connection refused" — does not say *what* it
                // tried to reach. The message a user reads in the TUI
                // now names the endpoint so the config file is the
                // obvious next place to look.
                KodError::Provider(format!(
                    "could not reach the model server at {url}: {e}"
                ))
            })?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            // Trim the body: an HTML error page from a wrong port is
            // hundreds of lines, and the first few carry the meaning.
            let body_short = if body.len() > 400 {
                format!("{}…", &body[..body.floor_char_boundary(400)])
            } else {
                body
            };
            return Err(KodError::Provider(format!(
                "list models failed: {url} returned {status}: {body_short}"
            )));
        }
        let body: serde_json::Value = response
            .json()
            .await
            .map_err(|e| KodError::Provider(format!("invalid models response: {e}")))?;
        Ok(body
            .get("data")
            .and_then(|d| d.as_array())
            .map(|models| {
                models
                    .iter()
                    .filter_map(|m| m.get("id").and_then(|id| id.as_str()).map(String::from))
                    .collect()
            })
            .unwrap_or_default())
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

impl OpenAICompatProvider {
    /// Live-token SSE stream for a prepared request (text and/or tool calls).
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

/// Normalize a user-supplied endpoint to the OpenAI API root.
///
/// Accepts a server root (`http://localhost:11434`, `http://localhost:1234`)
/// or an API root that already ends in `/v1`.
pub fn normalize_base_url(base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    if trimmed.ends_with("/v1") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/v1")
    }
}

/// Resolve the API key: explicit value first, then `OPENAI_API_KEY`, then a
/// dummy value local servers accept.
fn resolve_api_key(explicit: Option<String>) -> String {
    if let Some(key) = explicit
        && !key.trim().is_empty()
    {
        return key;
    }
    match std::env::var("OPENAI_API_KEY") {
        Ok(key) if !key.trim().is_empty() => key,
        _ => LOCAL_FALLBACK_API_KEY.to_string(),
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

/// Convert kod tool definitions to adk tool declarations keyed by tool name.
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
