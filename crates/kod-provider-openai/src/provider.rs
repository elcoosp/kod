//! `LlmProvider` implementation over `adk_model::openai_compatible::OpenAICompatible`.

use adk_core::{Content, GenerateContentConfig, Llm, LlmRequest, Part};
use adk_model::openai_compatible::{OpenAICompatible, OpenAICompatibleConfig};
use async_trait::async_trait;
use futures::{Stream, StreamExt};
use kod_error::{KodError, Result};
use kod_provider::request::CompletionRequest;
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

    /// Build an `adk-model` `LlmRequest` from a structured
    /// `CompletionRequest` (design §2 AD-01, ADR-04 Q1).
    ///
    /// One `Content` per source message, with the role the wire wants
    /// (`user`, `assistant`, `tool`, `system`). The system prompt is a
    /// leading `system` content; adk-model's converters extract it into
    /// the top-level `system` field for providers that use one
    /// (Anthropic, Gemini, Bedrock) and leave it as a role message for
    /// OpenAI-compatible ones, which is exactly the OpenAI spec.
    ///
    /// Tool calls and tool results are attached as `Part::FunctionCall`
    /// and `Part::FunctionResponse` respectively. The multi-role
    /// support was the ADR-04 Q1 spike's headline finding; this method
    /// is the first production use of it.
    fn request_from_completion(&self, req: &CompletionRequest) -> LlmRequest {
        use kod_types::MessageRole;

        let mut contents: Vec<Content> = Vec::new();

        // System prompt, if any. adk-model's per-provider converters
        // recognise `role == "system"` and either lift it into the
        // top-level field or keep it as a role message.
        let sys_text = req.system.render_text();
        if !sys_text.is_empty() {
            contents.push(Content::new("system").with_text(sys_text));
        }

        for m in &req.messages {
            match &m.role {
                MessageRole::User => {
                    contents.push(Content::new("user").with_text(&m.content));
                }
                MessageRole::Assistant | MessageRole::Agent(_) => {
                    // Assistant message: text form only. adk-model
                    // 2.2 does not surface `Part::FunctionCall` on
                    // the OpenAI-compatible wire — a Content that
                    // carries both text and FunctionCall parts loses
                    // the text (the FunctionCall path silently drops
                    // the whole content). Emitting the metadata as
                    // plain text is the only form that reliably
                    // reaches the server, and it carries the id so a
                    // subsequent tool_result can be linked to its
                    // call.
                    let mut text = m.content.clone();
                    for call in &m.tool_calls {
                        let id = call.id.as_deref().unwrap_or("");
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(&format!(
                            "[tool_call id={id} name={}] {}",
                            call.tool_name, call.arguments,
                        ));
                    }
                    let c = if text.is_empty() {
                        Content::new("assistant")
                    } else {
                        Content::new("assistant").with_text(&text)
                    };
                    contents.push(c);
                }
                MessageRole::Tool => {
                    // A tool result. adk-core 2.2's OpenAI-compatible
                    // converter drops a `tool`-role `Content` that
                    // carries only text — the wire test in
                    // `tests/contracts.rs` proved the body never
                    // reached the socket (both the tool content and
                    // the `tool_call_id` annotation were absent).
                    //
                    // Until the native `FunctionResponse` shape is
                    // pinned, the tool result therefore travels as a
                    // `user`-role text `Content` with an explicit
                    // `[tool_result tool_call_id=…]` prefix. The
                    // link back to the originating call is preserved
                    // because the assistant message already emits
                    // `[tool_call id=… name=…]` on the wire, so both
                    // halves of the pair carry the same id.
                    let id = m.tool_call_id.clone().unwrap_or_default();
                    let annotated = if id.is_empty() {
                        format!("[tool_result]\n{}", m.content)
                    } else {
                        format!("[tool_result tool_call_id={id}]\n{}", m.content)
                    };
                    contents.push(Content::new("user").with_text(annotated));
                }
                MessageRole::System => {
                    // A `System` message inside the transcript is a
                    // caller bug — the system prompt belongs in
                    // `req.system`. Dropped rather than emit a
                    // second system turn, which the wire would
                    // reject.
                }
            }
        }

        let mut request = LlmRequest::new(self.request_model(&req.options), contents)
            .with_config(options_to_config(&req.options));
        if !req.tools.is_empty() {
            request.tools = tool_declarations(&req.tools);
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
                            id: None,
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
                KodError::Provider(format!("could not reach the model server at {url}: {e}"))
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

    /// Structured completion (AD-01): builds the wire from
    /// `CompletionRequest` (multi-role messages, tool calls with ids,
    /// tool results linked by `tool_call_id`, system prompt).
    async fn complete(&self, req: &CompletionRequest) -> Result<GenerationResponse> {
        let request = self.request_from_completion(req);
        let (text, calls, usage) = self.collect(request, false).await?;
        if calls.is_empty() {
            Ok(GenerationResponse::Text { content: text, usage })
        } else if text.is_empty() {
            Ok(GenerationResponse::ToolCalls { calls, usage })
        } else {
            Ok(GenerationResponse::Mixed { content: text, calls, usage })
        }
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

    /// Structured streaming (design §2 AD-01, §4 D1.4 PR A3).
    ///
    /// Same SSE machinery as `stream_with_tools`, but the request is
    /// built from `CompletionRequest` (multi-role messages, tool calls
    /// with ids, tool results with `tool_call_id`) rather than a
    /// rendered text prompt. The `LlmRequest` is owned by the returned
    /// stream, so the caller's borrow on `req` ends at the call.
    fn stream_completion<'a>(
        &'a self,
        req: &'a CompletionRequest,
    ) -> Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send + 'a>> {
        let request = self.request_from_completion(req);
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

#[cfg(test)]
mod coverage_openai_helpers {
    //! Coverage for the pure helpers at the top of the file. These
    //! are the functions most likely to be silently broken by a
    //! refactor — the retry classifier in particular, since a wrong
    //! `is_retryable` answer means either "hang forever on a 401"
    //! or "give up on a rate limit" and both are bad in different
    //! directions.
    use super::*;

    // ---- is_retryable --------------------------------------------------

    #[test]
    fn is_retryable_recognizes_rate_limit_variants() {
        assert!(is_retryable("rate limit exceeded"));
        assert!(is_retryable("HTTP 429: Too Many Requests"));
        assert!(is_retryable("too many requests"));
    }

    #[test]
    fn is_retryable_recognizes_timeout_and_connection_errors() {
        assert!(is_retryable("request timeout"));
        assert!(is_retryable("connection timed out"));
        assert!(is_retryable("connection reset by peer"));
        assert!(is_retryable("connection closed before response"));
    }

    #[test]
    fn is_retryable_recognizes_server_errors() {
        assert!(is_retryable("503 Service Unavailable"));
        assert!(is_retryable("502 Bad Gateway"));
        assert!(is_retryable("504 Gateway Timeout"));
        assert!(is_retryable("server temporarily unavailable"));
        assert!(is_retryable("please try again"));
    }

    #[test]
    fn is_retryable_is_case_insensitive() {
        assert!(is_retryable("RATE LIMIT"));
        assert!(is_retryable("Timeout"));
        assert!(is_retryable("BAD GATEWAY"));
    }

    #[test]
    fn is_retryable_rejects_auth_and_not_found_errors() {
        // Retrying these wastes the user's time and hides the real
        // problem behind a delay.
        assert!(!is_retryable("401 Unauthorized"));
        assert!(!is_retryable("invalid api key"));
        assert!(!is_retryable("404 Not Found"));
        assert!(!is_retryable("model not found"));
        assert!(!is_retryable("400 Bad Request"));
    }

    #[test]
    fn is_retryable_rejects_empty_and_unrelated_messages() {
        assert!(!is_retryable(""));
        assert!(!is_retryable("everything is fine"));
        assert!(!is_retryable("some other problem"));
    }

    // ---- normalize_base_url --------------------------------------------

    #[test]
    fn normalize_base_url_appends_v1_when_missing() {
        assert_eq!(
            normalize_base_url("http://localhost:11434"),
            "http://localhost:11434/v1"
        );
        assert_eq!(
            normalize_base_url("http://localhost:1234"),
            "http://localhost:1234/v1"
        );
    }

    #[test]
    fn normalize_base_url_keeps_existing_v1() {
        assert_eq!(
            normalize_base_url("https://api.openai.com/v1"),
            "https://api.openai.com/v1"
        );
    }

    #[test]
    fn normalize_base_url_trims_trailing_slashes_before_appending() {
        assert_eq!(
            normalize_base_url("http://localhost:11434/"),
            "http://localhost:11434/v1"
        );
        assert_eq!(
            normalize_base_url("https://api.openai.com/v1/"),
            "https://api.openai.com/v1"
        );
        assert_eq!(
            normalize_base_url("https://api.openai.com/v1///"),
            "https://api.openai.com/v1"
        );
    }

    #[test]
    fn normalize_base_url_handles_a_bare_host() {
        // No scheme — the function does not validate, only normalises.
        assert_eq!(normalize_base_url("api.example.com"), "api.example.com/v1");
    }

    // ---- resolve_api_key -----------------------------------------------

    #[test]
    fn resolve_api_key_prefers_an_explicit_non_empty_value() {
        assert_eq!(resolve_api_key(Some("sk-explicit".into())), "sk-explicit");
    }

    #[test]
    fn resolve_api_key_rejects_a_whitespace_only_explicit_value() {
        // A config file with `api_key = "   "` must fall through to
        // the env var or the fallback, not be sent as a bearer token.
        let key = resolve_api_key(Some("   ".into()));
        assert_ne!(key, "   ", "whitespace-only must not be returned");
        assert!(!key.is_empty(), "a key is always returned");
    }

    #[test]
    fn resolve_api_key_returns_a_non_empty_string_for_none() {
        // Either the env var (whatever it is in this test process)
        // or the local fallback. Never empty — the OpenAI spec
        // servers reject a missing Authorization header outright,
        // so an empty string would be a hard failure for a user
        // who configured no key at all.
        let key = resolve_api_key(None);
        assert!(!key.is_empty());
    }

    #[test]
    fn local_fallback_api_key_is_the_documented_sentinel() {
        // The value is what local servers (Ollama, LM Studio) see
        // when no key is configured. Changing it is harmless but
        // the constant should not silently become empty.
        assert_eq!(LOCAL_FALLBACK_API_KEY, "not-needed");
    }

    // ---- consts --------------------------------------------------------

    #[test]
    fn retry_constants_have_sane_values() {
        // `MAX_RETRIES` must be > 1 (one retry is not enough for the
        // Ollama cold-start case the doc comment names) and the
        // backoff must be sub-second so the user sees a retry, not
        // a hang.
        assert!(MAX_RETRIES >= 2, "MAX_RETRIES = {MAX_RETRIES}");
        assert!(RETRY_BACKOFF_MS > 0);
        assert!(RETRY_BACKOFF_MS < 5_000);
    }
}

#[cfg(test)]
mod coverage_openai_provider {
    //! Coverage for provider construction and the accessors that
    //! the TUI and CLI use to display and switch the endpoint. The
    //! HTTP path (`collect`, `stream_request`) needs a mock server
    //! and lives in `tests/contracts.rs`; these tests never touch
    //! the network.
    use super::*;

    // ---- with_api_key --------------------------------------------------

    #[test]
    fn with_api_key_normalizes_a_bare_host() {
        let p = OpenAICompatProvider::with_api_key(
            "http://localhost:11434",
            "qwen2.5-coder:7b",
            "not-needed",
        )
        .unwrap();
        assert_eq!(p.base_url(), "http://localhost:11434/v1");
    }

    #[test]
    fn with_api_key_keeps_an_existing_v1_suffix() {
        let p = OpenAICompatProvider::with_api_key(
            "https://api.openai.com/v1",
            "gpt-5",
            "sk-test",
        )
        .unwrap();
        assert_eq!(p.base_url(), "https://api.openai.com/v1");
    }

    #[test]
    fn with_api_key_stores_the_default_model() {
        let p = OpenAICompatProvider::with_api_key(
            "http://localhost:11434",
            "llama3.3:70b",
            "not-needed",
        )
        .unwrap();
        assert_eq!(p.default_model(), "llama3.3:70b");
    }

    // ---- with_model ----------------------------------------------------

    #[test]
    fn with_model_switches_the_model_and_keeps_the_endpoint() {
        let p = OpenAICompatProvider::with_api_key(
            "http://localhost:11434",
            "qwen2.5-coder:7b",
            "not-needed",
        )
        .unwrap();
        let p2 = p.with_model("qwen2.5-coder:32b").unwrap();
        assert_eq!(p2.default_model(), "qwen2.5-coder:32b");
        assert_eq!(
            p2.base_url(),
            "http://localhost:11434/v1",
            "a model switch must not change the endpoint",
        );
    }

    #[test]
    fn with_model_can_be_chained() {
        // Three back-to-back switches must all succeed and land on
        // the last model. This is the shape of a user cycling
        // models in the TUI.
        let p = OpenAICompatProvider::with_api_key(
            "http://localhost:11434",
            "m1",
            "not-needed",
        )
        .unwrap();
        let p = p.with_model("m2").unwrap();
        let p = p.with_model("m3").unwrap();
        assert_eq!(p.default_model(), "m3");
        assert_eq!(p.base_url(), "http://localhost:11434/v1");
    }

    // ---- with_api_key_and_timeout --------------------------------------

    #[test]
    fn with_api_key_and_timeout_accepts_a_custom_timeout() {
        // The timeout is stored (not just used to build the client)
        // and carried forward by `with_model`.
        let p = OpenAICompatProvider::with_api_key_and_timeout(
            "http://localhost:11434",
            "m",
            "not-needed",
            42,
        )
        .unwrap();
        assert_eq!(p.timeout_secs, 42);
        let p2 = p.with_model("m2").unwrap();
        assert_eq!(
            p2.timeout_secs, 42,
            "with_model must carry the timeout forward",
        );
    }

    #[test]
    fn with_api_key_defaults_the_timeout_to_300() {
        let p = OpenAICompatProvider::with_api_key(
            "http://localhost:11434",
            "m",
            "not-needed",
        )
        .unwrap();
        assert_eq!(p.timeout_secs, 300);
    }

    // ---- options_to_config ---------------------------------------------

    #[test]
    fn options_to_config_carries_temperature_and_top_p() {
        let opts = GenerationOptions {
            temperature: Some(0.7),
            top_p: Some(0.9),
            ..Default::default()
        };
        let cfg = options_to_config(&opts);
        assert_eq!(cfg.temperature, Some(0.7));
        assert_eq!(cfg.top_p, Some(0.9));
    }

    #[test]
    fn options_to_config_defaults_to_none_when_options_are_unset() {
        let opts = GenerationOptions::default();
        let cfg = options_to_config(&opts);
        assert!(cfg.temperature.is_none());
        assert!(cfg.top_p.is_none());
    }

    #[test]
    fn options_to_config_maps_max_tokens_through_i32() {
        let opts = GenerationOptions {
            max_tokens: Some(1024),
            ..Default::default()
        };
        let cfg = options_to_config(&opts);
        assert_eq!(cfg.max_output_tokens, Some(1024));
    }

    #[test]
    fn options_to_config_drops_a_max_tokens_that_overflows_i32() {
        // `u64::MAX` cannot fit in an `i32`; the `and_then` in the
        // implementation drops it to `None` rather than truncating
        // to a garbage value. Truncation would send the server a
        // negative or tiny token budget.
        let opts = GenerationOptions {
            max_tokens: Some(usize::MAX),
            ..Default::default()
        };
        let cfg = options_to_config(&opts);
        assert!(
            cfg.max_output_tokens.is_none(),
            "overflowing max_tokens must be dropped, got {:?}",
            cfg.max_output_tokens,
        );
    }

    // ---- name ----------------------------------------------------------

    #[test]
    fn provider_name_is_stable() {
        // The name is displayed in the TUI's header and used by the
        // registry; changing it is a visible change.
        let p = OpenAICompatProvider::with_api_key(
            "http://localhost:11434",
            "m",
            "not-needed",
        )
        .unwrap();
        assert_eq!(p.name(), "openai-compatible");
    }
}
