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
use std::sync::Arc;

/// Fallback API key for local servers (Ollama, LM Studio, MLX) that accept any value.
const LOCAL_FALLBACK_API_KEY: &str = "not-needed";

/// Retries attempted on a transient provider error (rate limit, 5xx,
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
    /// Per-endpoint streaming concurrency bracket (§9.9). The cap is
    /// held only around the streaming HTTP request itself, never
    /// around the agent's lifetime; see `kod_provider::concurrency`
    /// for why that distinction is the deadlock fix in issue #3749.
    /// `0` means unbounded, which is the default.
    concurrency: Arc<kod_provider::concurrency::ProviderConcurrency>,
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
            concurrency: Arc::new(
                kod_provider::concurrency::ProviderConcurrency::new(0),
            ),
        })
    }

    /// Set the per-endpoint streaming concurrency cap (§9.9).
    ///
    /// `0` is unbounded and is the default — a caller that never
    /// touches this method sees exactly the behavior it saw before
    /// the primitive existed. A caller that sets a positive cap
    /// wraps **only the streaming HTTP request** in the bracket; the
    /// agent's own lifetime is unrelated to slot occupancy.
    pub fn with_concurrency(self, cap: usize) -> Self {
        self.concurrency.set_cap(cap);
        self
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
            concurrency: self.concurrency,
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
        use adk_core::{FunctionResponseData, Part};
        use kod_types::MessageRole;

        let mut contents: Vec<Content> = Vec::new();

        // System prompt, if any. adk-model's per-provider converters
        // recognise `role == "system"` and lift it into the
        // top-level field.
        let sys_text = req.system.render_text();
        if !sys_text.is_empty() {
            contents.push(Content::new("system").with_text(sys_text));
        }

        // H-P1: use the *native* OpenAI wire shapes. adk-model 2.2
        // emits them correctly when the right `Part` variants are
        // used:
        //
        //   role "assistant" + Part::FunctionCall { name, args, id }
        //     → tool_calls[] on the request body.
        //   role "tool" + first Part::FunctionResponse { .., id }
        //     → { role: "tool", tool_call_id: id, content: text }.
        //
        // The pre-fix shape flattened both halves into plain text
        // (`[tool_call id=… name=…]` / `[tool_result …]`) because a
        // previous maintainer believed adk silently dropped the
        // parts. That is not true — the parts reach the wire intact
        // as long as the role is one of the recognised role strings
        // and FunctionCall/FunctionResponse is the *only* kind of
        // non-text part in the content.

        for m in &req.messages {
            match &m.role {
                MessageRole::User => {
                    contents.push(Content::new("user").with_text(&m.content));
                }
                MessageRole::Assistant | MessageRole::Agent(_) => {
                    // Build the assistant turn as text (if any) plus
                    // one FunctionCall part per tool call. `adk-model`'s
                    // OpenAI converter extracts the tool_calls from
                    // the parts list and emits them on the wire; the
                    // text becomes the assistant's `content` field.
                    let mut c = Content::new("assistant");
                    if !m.content.is_empty() {
                        c.parts.push(Part::Text {
                            text: m.content.clone(),
                        });
                    }
                    for call in &m.tool_calls {
                        c.parts.push(Part::FunctionCall {
                            name: call.tool_name.clone(),
                            args: call.arguments.clone(),
                            id: call.id.clone(),
                            thought_signature: None,
                        });
                    }
                    // OpenAI rejects an assistant message with no
                    // content and no tool_calls. If both are empty,
                    // emit a single space — the minimal non-empty
                    // content the converter's own fallback uses.
                    if c.parts.is_empty() {
                        c.parts.push(Part::Text {
                            text: " ".to_string(),
                        });
                    }
                    contents.push(c);
                }
                MessageRole::Tool => {
                    // Tool result: role "tool" with one
                    // FunctionResponse part whose `id` is the
                    // tool_call_id. `adk-model`'s converter reads
                    // `parts.first()` as the response and emits
                    // {role:"tool", tool_call_id: id, content: text}.
                    //
                    // The FunctionResponseData carries a name; the
                    // caller-supplied tool result does not, so we
                    // synthesize "tool" — the OpenAI converter does
                    // not use the name on the wire, only the id and
                    // the serialized response payload.
                    let id = m
                        .tool_call_id
                        .clone()
                        .unwrap_or_else(|| "unknown".to_string());
                    let data = FunctionResponseData::new(
                        "tool",
                        serde_json::json!({ "result": m.content }),
                    );
                    let mut c = Content::new("tool");
                    c.parts.push(Part::FunctionResponse {
                        function_response: data,
                        id: Some(id),
                        annotations: None,
                    });
                    contents.push(c);
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
        // H-P3: route through the shared `RetryPolicy`. The pre-fix
        // inline loop used a substring classifier (`is_retryable`) on
        // the formatted error string, which matches permanent 401 /
        // 403 bodies that happen to contain "try again" and had no
        // jitter — a swarm of agents hit a 429 simultaneously and
        // retried in lockstep. The shared policy classifies from the
        // typed `KodError` (`is_retryable()`), honours `Retry-After`
        // on a `RateLimited`, and jitters the backoff.
        let policy = kod_provider::retry::RetryPolicy::default();
        kod_provider::retry::with_retry(&policy, || {
            let req = request.clone();
            async move { self.collect_once(&req, stream).await }
        })
        .await
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
                    // Subset convention: `prompt_token_count` already
                    // includes cached tokens; the cache field is the
                    // *subset* served from cache.
                    cache_read_tokens: usage
                        .cache_read_input_token_count
                        .map(|n| n.max(0) as u64),
                    cache_creation_tokens: usage
                        .cache_creation_input_token_count
                        .map(|n| n.max(0) as u64),
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

    async fn list_models(&self) -> Result<Vec<kod_provider::ModelInfo>> {
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
                format!(
                    "{}…",
                    &body[..kod_types::strutil::floor_char_boundary(&body, 400)]
                )
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
                    .filter_map(|m| m.get("id").and_then(|id| id.as_str()).map(kod_provider::ModelInfo::bare))
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
    /// Override the conservative default with the matrix this
    /// provider actually implements (harness review section 9):
    /// OpenAI-compatible servers prefix-cache server-side with no
    /// client hint, and this stream path emits live text during a
    /// tool call. The conservative default claimed neither.
    fn capabilities(&self) -> kod_provider::ProviderCapabilities {
        let mut caps = kod_provider::ProviderCapabilities::conservative();
        caps.prompt_cache = kod_provider::PromptCacheKind::Automatic;
        caps.streaming_tools = true;
        caps
    }

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
        let concurrency = Arc::clone(&self.concurrency);
        Box::pin(async_stream::stream! {
            // §9.9: hold one admission permit for the lifetime of the
            // streaming HTTP request. Released when this stream body
            // exits — normal completion, hard error, or the caller
            // dropping the stream. The bracket never extends past the
            // stream; see `kod_provider::concurrency`.
            let _permit = concurrency.acquire().await;
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
                                        cache_read_tokens: usage
                                            .cache_read_input_token_count
                                            .map(|n| n.max(0) as u64),
                                        cache_creation_tokens: usage
                                            .cache_creation_input_token_count
                                            .map(|n| n.max(0) as u64),
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
                                // H-P10: `return` after the error. The
                                // stream contract (traits.rs) says an
                                // Err means the turn is over. The pre-fix
                                // `break` fell through to the trailing
                                // `Usage` + `Done`, so a consumer that
                                // saw `Err` then `Done` could not tell a
                                // hard failure from a clean stop.
                                yield Err(adk_err(e));
                                return;
                            }
                        }
                    }
                    if let Some(usage) = last_usage {
                        yield Ok(StreamChunk::Usage(usage));
                    }
                    yield Ok(StreamChunk::Done);
                }
                Err(e) => {
                    // H-P10: return rather than fall through to `Done`.
                    yield Err(adk_err(e));
                    return;
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
    use super::*;
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
}

#[cfg(test)]
mod coverage_openai_provider {
    //! Coverage for provider construction and the accessors that
    //! the TUI and CLI use to display and switch the endpoint. The
    //! HTTP path (`collect`, `stream_request`) needs a mock server
    //! and lives in `tests/contracts.rs`; these tests never touch
    //! the network.
    use super::*;
    use kod_provider::GenerationOptions;

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
        let p = OpenAICompatProvider::with_api_key("https://api.openai.com/v1", "gpt-5", "sk-test")
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
        let p = OpenAICompatProvider::with_api_key("http://localhost:11434", "m1", "not-needed")
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
        let p = OpenAICompatProvider::with_api_key("http://localhost:11434", "m", "not-needed")
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
        let p = OpenAICompatProvider::with_api_key("http://localhost:11434", "m", "not-needed")
            .unwrap();
        assert_eq!(p.name(), "openai-compatible");
    }

    #[test]
    fn native_tool_call_round_trip_uses_wire_shape() {
        // H-P1: an assistant turn with a tool call + a tool result
        // must produce Part::FunctionCall and Part::FunctionResponse
        // — not plain text. This test builds the two contents
        // directly and asserts the parts are present.
        use adk_core::{Content, FunctionResponseData, Part};
        use kod_provider::request::{CompletionRequest, ModelRef, SystemPrompt};
        use kod_types::{ChatMessage, MessageId, MessageRole, ToolCall};
        use time::OffsetDateTime;

        let p = OpenAICompatProvider::with_api_key("http://localhost:11434", "m", "not-needed")
            .unwrap();

        let now = OffsetDateTime::now_utc();
        let mut assistant = ChatMessage::text(
            MessageId::new(),
            MessageRole::Assistant,
            "let me check",
            now,
        );
        assistant.tool_calls.push(ToolCall {
            id: Some("call_1".to_string()),
            tool_name: "read_file".to_string(),
            arguments: serde_json::json!({ "path": "a.rs" }),
        });
        let mut tool = ChatMessage::text(MessageId::new(), MessageRole::Tool, "file contents", now);
        tool.tool_call_id = Some("call_1".to_string());

        let req = CompletionRequest {
            model: ModelRef::new("default", "m"),
            messages: vec![assistant, tool],
            system: SystemPrompt::default(),
            tools: vec![],
            options: Default::default(),
            cache_transcript: true,
        };

        let llm = p.request_from_completion(&req);
        assert_eq!(llm.contents.len(), 2);

        // Assistant half: FunctionCall part.
        let a = &llm.contents[0];
        assert_eq!(a.role, "assistant");
        assert!(
            a.parts.iter().any(|p| matches!(
                p,
                Part::FunctionCall { name, id, .. }
                    if name == "read_file"
                        && id.as_deref() == Some("call_1"),
            )),
            "assistant FunctionCall part missing: {:?}",
            a.parts,
        );

        // Tool half: FunctionResponse with the matching id.
        let t = &llm.contents[1];
        assert_eq!(t.role, "tool");
        assert!(
            t.parts.iter().any(|p| matches!(
                p,
                Part::FunctionResponse { id, .. }
                    if id.as_deref() == Some("call_1"),
            )),
            "tool FunctionResponse part missing: {:?}",
            t.parts,
        );
        // Silence unused import (used only in docs above).
        let _ = FunctionResponseData::new("x", serde_json::json!({}));
        let _ = Content::new("user");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_declare_automatic_prompt_cache() {
        // OpenAI-compatible servers (OpenAI, Ollama, vLLM, ...) prefix-cache
        // automatically and there is no cache_control marker to send. Kod
        // must therefore advertise Automatic caching rather than None, so
        // the engine knows cache-read tokens are possible and does not gate
        // transcript breakpoints on this provider.
        use kod_provider::PromptCacheKind;
        let provider = OpenAICompatProvider::new(
            "http://localhost:11434/v1",
            "test-model",
        )
        .expect("constructing an OpenAI-compatible provider must succeed");
        let caps = provider.capabilities();
        assert_eq!(
            caps.prompt_cache,
            PromptCacheKind::Automatic,
            "OpenAI-compatible provider must declare Automatic prompt cache",
        );
        assert!(
            caps.streaming_tools,
            "OpenAI-compatible provider streams tool-call text",
        );
    }

    #[test]
    fn openai_usage_carries_cache_subset() {
        // The Subset convention: `prompt_tokens` includes cached
        // tokens, so the cached portion must be subtracted before
        // billing. This pins that the cost math honours it — a
        // regression that reverted to Split would over-bill every
        // cached turn at the full input rate.
        use kod_provider::request::ModelPricing;
        use kod_provider::CacheConvention;
        use kod_provider::TokenUsage;

        let usage = TokenUsage {
            prompt_tokens: 10_000,
            completion_tokens: 500,
            total_tokens: 10_500,
            cache_read_tokens: Some(8_000),
            cache_creation_tokens: Some(0),
        };
        let pricing = ModelPricing::new(3.0, 15.0)
            .with_cache_convention(CacheConvention::Subset);
        let cost = pricing.cost_for_usage(&usage);
        // fresh = 10k - 8k = 2k → 0.006
        // read  = 8k → 0.0024
        // completion = 500 → 0.0075
        // total ≈ 0.0159
        assert!((cost - 0.0159).abs() < 1e-6, "cost = {cost}");
    }
}
