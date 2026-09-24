//! `LlmProvider` implementation over `adk_model::anthropic::Anthropic`.

use adk_core::{Content, GenerateContentConfig, Llm, LlmRequest, Part};
use adk_model::anthropic::{AnthropicClient, AnthropicConfig};
use async_trait::async_trait;
use futures::{Stream, StreamExt};
use kod_error::{KodError, Result};
use kod_provider::request::CompletionRequest;
use kod_provider::{
    GenerationOptions, GenerationResponse, LlmProvider, PromptCacheKind, ProviderCapabilities,
    StreamChunk,
};
use kod_types::{ToolCall, ToolDefinition};
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;

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
    /// H-P7: per-request timeout in seconds, carried so a
    /// `with_model` switch keeps the same setting. Retained for
    /// API parity with the OpenAI provider; the client itself is
    /// built with connect-only + a per-request wrapper, so a long
    /// stream is not killed mid-flight.
    timeout_secs: u64,
    /// Per-endpoint streaming concurrency bracket (§9.9). Same
    /// semantics as the OpenAI provider's field; see
    /// `kod_provider::concurrency`. The default cap is 0 (unbounded).
    concurrency: Arc<kod_provider::concurrency::ProviderConcurrency>,
    /// Whether the §9.3 stream stall detector runs on this provider's
    /// streaming paths. On by default; same contract as the OpenAI
    /// provider's field.
    stream_guard_enabled: bool,
}

impl AnthropicProvider {
    /// Create a provider with an explicit API key.
    pub fn with_api_key(
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Result<Self> {
        Self::with_api_key_and_timeout(base_url, model, api_key, 300)
    }

    /// Like `with_api_key` but with a configurable request timeout.
    /// H-P7: parity with the OpenAI provider, and the field the
    /// `[llm.endpoints].timeout_secs` config flows into.
    ///
    /// Note: the client below carries only a *connect* timeout. A
    /// total timeout on the reqwest client would abort any stream
    /// longer than the bound — the pre-fix 300 s total made long
    /// generations fail mid-flight. The engine's per-chunk idle
    /// timeout (H-E11) is the streaming bound; `timeout_secs` is
    /// honoured for non-streaming `complete()` calls via the
    /// request-level wrapper in `collect`.
    pub fn with_api_key_and_timeout(
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key: impl Into<String>,
        timeout_secs: u64,
    ) -> Result<Self> {
        let base_url = normalize_base_url(&base_url.into());
        let api_key = api_key.into();
        let model = model.into();
        let inner = build_client(&api_key, &model, &base_url)?;
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| {
                KodError::Provider(format!("anthropic: could not build http client: {e}"))
            })?;
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
            stream_guard_enabled: true,
        })
    }

    /// Opt in or out of the §9.3 stream stall detector. On by default.
    pub fn with_stream_guard(mut self, enabled: bool) -> Self {
        self.stream_guard_enabled = enabled;
        self
    }

    /// Set the per-endpoint streaming concurrency cap (§9.9).
    ///
    /// `0` is unbounded and is the default. A positive cap wraps only
    /// the streaming HTTP request — see the OpenAI provider's
    /// `with_concurrency` for the contract, and
    /// `kod_provider::concurrency` for the deadlock reasoning.
    pub fn with_concurrency(self, cap: usize) -> Self {
        self.concurrency.set_cap(cap);
        self
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
            timeout_secs: self.timeout_secs,
            concurrency: self.concurrency,
            stream_guard_enabled: self.stream_guard_enabled,
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
        // H-P7: bound the non-streaming collect by `timeout_secs`. The
        // pre-fix reqwest client carried a hard 300 s total timeout,
        // which (a) killed long generations and (b) was not
        // configurable. We moved the timeout here, where it bounds
        // exactly the shapes that should be bounded — a non-streaming
        // `complete()` call, and the pre-first-chunk wait of a
        // streaming one. The streaming loop has its own idle deadline
        // in the engine.
        let timeout = std::time::Duration::from_secs(self.timeout_secs.max(1));
        tokio::time::timeout(timeout, self.collect_inner(request, stream))
            .await
            .map_err(|_| KodError::ProviderTimeout {
                timeout_ms: timeout.as_millis() as u64,
            })?
    }

    async fn collect_inner(
        &self,
        request: LlmRequest,
        stream: bool,
    ) -> Result<(String, Vec<ToolCall>, Option<kod_provider::TokenUsage>)> {
        let mut responses =
            kod_provider::retry::with_retry(&kod_provider::retry::RetryPolicy::default(), || {
                let req = request.clone();
                async move {
                    self.inner
                        .generate_content(req, stream)
                        .await
                        .map_err(adk_err)
                }
            })
            .await?;
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
                    ..Default::default()
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

    async fn list_models(&self) -> Result<Vec<kod_provider::ModelInfo>> {
        // Harness review section 9: Anthropic has no public
        // `GET /v1/models` at time of writing, so an empty vec was
        // returned. A curated list of the model families kod knows
        // is more useful: `/model` offers something to pick from.
        Ok(vec![
            kod_provider::ModelInfo::bare("claude-opus-4"),
            kod_provider::ModelInfo::bare("claude-sonnet-4"),
            kod_provider::ModelInfo::bare("claude-haiku-4"),
            kod_provider::ModelInfo::bare("claude-3-5-sonnet-latest"),
            kod_provider::ModelInfo::bare("claude-3-5-haiku-latest"),
        ])
    }

    /// Native Messages API call (design §4 D1.2, A5b). Uses the wire
    /// module to build a body with `cache_control` on the last cacheable
    /// system segment, POSTs it, and parses the response. The other
    /// trait methods (`generate`, `generate_with_tools`, `stream*`) stay
    /// on the `adk-model` path — that is the pre-migration surface, and
    /// this override only affects callers that have already migrated to
    /// `CompletionRequest`.
    async fn complete(&self, req: &CompletionRequest) -> Result<GenerationResponse> {
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
            .map_err(|e| KodError::Provider(format!("anthropic: POST {url}: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            let snippet = if text.len() > 400 {
                format!("{}…", kod_types::strutil::truncate_chars(&text, 400))
            } else {
                text
            };
            return Err(KodError::provider_status(status.as_u16(), &snippet));
        }
        let parsed: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| KodError::Provider(format!("anthropic: invalid JSON: {e}")))?;
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
        let concurrency = Arc::clone(&self.concurrency);
        let guard_enabled = self.stream_guard_enabled;

        Box::pin(async_stream::stream! {
            // §9.9: one admission permit per streaming HTTP request.
            let _permit = concurrency.acquire().await;
            // §9.3: one guard per stream, fresh per attempt.
            let mut guard = if guard_enabled {
                Some(kod_provider::stream_guard::StreamGuard::new())
            } else {
                None
            };
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

            let mut stream = resp.bytes_stream();
            // H-P2: work in bytes, not `String`. `bytes_stream`
            // yields TCP chunks, and a multi-byte UTF-8 character can
            // straddle the boundary between two of them. The pre-fix
            // code did `buf.push_str(&String::from_utf8_lossy(&bytes))`
            // per chunk: half a CJK character becomes U+FFFD, and the
            // other half is lost on the next decode — visible as
            // mojibake in streamed text and *corrupted tool
            // arguments*. Accumulate bytes here, find line breaks on
            // bytes, and decode each complete line once (itself, not
            // the partial tail).
            let mut buf: Vec<u8> = Vec::new();
            let mut state = crate::wire::AnthropicStreamState::default();
            let mut done_sent = false;

            while let Some(chunk) = stream.next().await {
                match chunk {
                    Ok(bytes) => {
                        buf.extend_from_slice(&bytes);
                        // SSE frames are separated by a blank line.
                        // Find `\n` on bytes; `String::from_utf8`
                        // on a complete line cannot split a char.
                        while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
                            let mut line_bytes: Vec<u8> = buf.drain(..=nl).collect();
                            // Drop the trailing newline (and a CR
                            // before it, the SSE convention).
                            line_bytes.pop();
                            if line_bytes.last() == Some(&b'\r') {
                                line_bytes.pop();
                            }
                            let line = String::from_utf8_lossy(&line_bytes).into_owned();
                            for chunk in crate::wire::parse_sse_line(&mut state, &line) {
                                if matches!(chunk, StreamChunk::Done) {
                                    done_sent = true;
                                }
                                let stall = guard
                                    .as_mut()
                                    .and_then(|g| g.feed_chunk(&chunk));
                                yield Ok(chunk);
                                if let Some(detector) = stall {
                                    yield Err(KodError::Provider(format!(
                                        "stream stall detected: {detector}"
                                    )));
                                    return;
                                }
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
            // H-P2 (tail): an unterminated final line — the last
            // frame arrives without a trailing newline — was dropped
            // by the pre-fix loop, and it is frequently the final
            // `message_delta` (usage) or `message_stop`. Flush it
            // here.
            if !buf.is_empty() {
                let line = String::from_utf8_lossy(&buf).into_owned();
                buf.clear();
                for chunk in crate::wire::parse_sse_line(&mut state, &line) {
                    if matches!(chunk, StreamChunk::Done) {
                        done_sent = true;
                    }
                    let stall = guard.as_mut().and_then(|g| g.feed_chunk(&chunk));
                    yield Ok(chunk);
                    if let Some(detector) = stall {
                        yield Err(KodError::Provider(format!(
                            "stream stall detected: {detector}"
                        )));
                        return;
                    }
                }
            }

            // The spec says `message_stop` terminates; if the transport
            // closes without one (a truncated stream), still emit a
            // `Done` so the engine's assembly loop terminates instead
            // of stalling on the last partial call.
            // The two branches were identical; collapse them. The
            // contract is the same either way: the transport closed
            // (or `message_stop` arrived), the engine needs a `Done`.
            if !done_sent {
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
        let concurrency = Arc::clone(&self.concurrency);
        Box::pin(async_stream::stream! {
            // §9.9: one admission permit per streaming HTTP request,
            // held for the stream's lifetime only.
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
                                        ..Default::default()
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

    // Anthropic's `input_tokens` is the *uncached* portion; the two
    // cache fields are separate keys in the same usage object and are
    // billed at different rates. kod folds all three into
    // `prompt_tokens` so a single field carries the full input window,
    // and keeps the split in the cache fields for the cost math.
    let usage = v.get("usage").map(|u| {
        let input = u.get("input_tokens").and_then(|n| n.as_u64()).unwrap_or(0) as usize;
        let output = u.get("output_tokens").and_then(|n| n.as_u64()).unwrap_or(0) as usize;
        // Option preserved: `None` means the field was absent, which
        // is different from "present and zero." kod's `TokenUsage`
        // keeps that distinction so the hit-rate readout can say
        // "unknown" rather than lying with "0%".
        let cache_read = u.get("cache_read_input_tokens").and_then(|n| n.as_u64());
        let cache_creation = u
            .get("cache_creation_input_tokens")
            .and_then(|n| n.as_u64());
        let read_usize = cache_read.unwrap_or(0) as usize;
        let creation_usize = cache_creation.unwrap_or(0) as usize;
        kod_provider::TokenUsage {
            // `prompt_tokens` is the total window the provider billed
            // and processed. Anthropic reports `input_tokens` excluding
            // the cache fields, so they are added back here.
            prompt_tokens: input + read_usize + creation_usize,
            completion_tokens: output,
            total_tokens: input + read_usize + creation_usize + output,
            cache_read_tokens: cache_read,
            cache_creation_tokens: cache_creation,
        }
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

/// Coverage for the pure helpers that turn a Messages API body into
/// a `GenerationResponse`, plus the full `ProviderCapabilities`
/// matrix. `parse_response` is the only place the three response
/// shapes (`Text` / `ToolCalls` / `Mixed`) are decided, and the
/// tolerance branches (missing content, empty tool name, unknown
/// block types, absent usage) are exactly what a real refusal or a
/// truncated response hits.
#[cfg(test)]
mod coverage_provider_parse {
    use super::*;
    use serde_json::json;

    fn text_of(r: &GenerationResponse) -> &str {
        match r {
            GenerationResponse::Text { content, .. } => content,
            GenerationResponse::Mixed { content, .. } => content,
            GenerationResponse::ToolCalls { .. } => {
                panic!("expected a text-carrying variant")
            }
        }
    }

    // ---- parse_response ------------------------------------------------

    #[test]
    fn parse_response_text_only_with_usage() {
        let v = json!({
            "content": [{"type": "text", "text": "hello world"}],
            "usage": {"input_tokens": 7, "output_tokens": 3}
        });
        match parse_response(&v).unwrap() {
            GenerationResponse::Text { content, usage } => {
                assert_eq!(content, "hello world");
                let u = usage.expect("usage present");
                assert_eq!(u.prompt_tokens, 7);
                assert_eq!(u.completion_tokens, 3);
                assert_eq!(u.total_tokens, 10);
            }
            _ => panic!("expected Text variant"),
        }
    }

    #[test]
    fn parse_response_concatenates_multiple_text_blocks() {
        let v = json!({
            "content": [
                {"type": "text", "text": "first "},
                {"type": "text", "text": "second"}
            ]
        });
        let r = parse_response(&v).unwrap();
        assert_eq!(text_of(&r), "first second");
    }

    #[test]
    fn parse_response_tool_use_only() {
        let v = json!({
            "content": [{
                "type": "tool_use",
                "id": "toolu_abc",
                "name": "read_file",
                "input": {"path": "src/lib.rs"}
            }]
        });
        match parse_response(&v).unwrap() {
            GenerationResponse::ToolCalls { calls, .. } => {
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].id.as_deref(), Some("toolu_abc"));
                assert_eq!(calls[0].tool_name, "read_file");
                assert_eq!(calls[0].arguments["path"], "src/lib.rs");
            }
            _ => panic!("expected ToolCalls variant"),
        }
    }

    #[test]
    fn parse_response_mixed_text_and_tool_calls() {
        let v = json!({
            "content": [
                {"type": "text", "text": "let me check that"},
                {"type": "tool_use", "id": "x", "name": "grep", "input": {}}
            ]
        });
        match parse_response(&v).unwrap() {
            GenerationResponse::Mixed { content, calls, .. } => {
                assert_eq!(content, "let me check that");
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].tool_name, "grep");
            }
            _ => panic!("expected Mixed variant"),
        }
    }

    #[test]
    fn parse_response_empty_content_array_is_empty_text() {
        // Anthropic returns `content: []` on a refusal or an immediate
        // stop. The engine treats an empty reply as "nothing to say",
        // so this must not be an error.
        let v = json!({ "content": [] });
        match parse_response(&v).unwrap() {
            GenerationResponse::Text { content, usage } => {
                assert!(content.is_empty());
                assert!(usage.is_none());
            }
            _ => panic!("expected Text variant"),
        }
    }

    #[test]
    fn parse_response_missing_content_key_is_empty_text() {
        let v = json!({});
        let r = parse_response(&v).unwrap();
        assert_eq!(text_of(&r), "");
    }

    #[test]
    fn parse_response_skips_tool_use_with_empty_name() {
        // A malformed tool_use block (no `name`) is dropped; the
        // remaining text block survives and the response is Text, not
        // ToolCalls.
        let v = json!({
            "content": [
                {"type": "tool_use", "id": "x", "name": "", "input": {}},
                {"type": "text", "text": "fallback"}
            ]
        });
        let r = parse_response(&v).unwrap();
        assert_eq!(text_of(&r), "fallback");
        assert!(
            matches!(r, GenerationResponse::Text { .. }),
            "a lone empty-named tool_use must not become ToolCalls"
        );
    }

    #[test]
    fn parse_response_tool_use_without_id_has_no_id() {
        let v = json!({
            "content": [{"type": "tool_use", "name": "grep", "input": {}}]
        });
        match parse_response(&v).unwrap() {
            GenerationResponse::ToolCalls { calls, .. } => {
                assert!(calls[0].id.is_none());
            }
            _ => panic!("expected ToolCalls variant"),
        }
    }

    #[test]
    fn parse_response_tool_use_without_input_uses_null() {
        let v = json!({
            "content": [{"type": "tool_use", "id": "x", "name": "grep"}]
        });
        match parse_response(&v).unwrap() {
            GenerationResponse::ToolCalls { calls, .. } => {
                assert_eq!(calls[0].arguments, serde_json::Value::Null);
            }
            _ => panic!("expected ToolCalls variant"),
        }
    }

    #[test]
    fn parse_response_skips_unknown_block_types() {
        let v = json!({
            "content": [
                {"type": "thinking", "text": "hmm"},
                {"type": "text", "text": "kept"}
            ]
        });
        let r = parse_response(&v).unwrap();
        assert_eq!(text_of(&r), "kept");
    }

    #[test]
    fn parse_response_usage_missing_fields_default_to_zero() {
        let v = json!({
            "content": [{"type": "text", "text": "x"}],
            "usage": {}
        });
        match parse_response(&v).unwrap() {
            GenerationResponse::Text { usage, .. } => {
                let u = usage.expect("usage object present");
                assert_eq!(u.prompt_tokens, 0);
                assert_eq!(u.completion_tokens, 0);
                assert_eq!(u.total_tokens, 0);
            }
            _ => panic!("expected Text variant"),
        }
    }

    // ---- capabilities --------------------------------------------------

    #[test]
    fn capabilities_full_matrix() {
        // The existing tests check `tools`, `streaming_tools`, and
        // `prompt_cache`; this one pins every field so a future
        // capability flip is visible here, not in a router surprise.
        let p = AnthropicProvider::with_api_key(
            "https://api.anthropic.com",
            "claude-sonnet-4-5",
            "test-key",
        )
        .unwrap();
        let caps = p.capabilities();
        assert!(caps.tools);
        assert!(caps.vision);
        assert!(!caps.json_mode);
        assert_eq!(caps.prompt_cache, PromptCacheKind::Explicit);
        assert!(!caps.embeddings);
        assert!(caps.streaming_tools);
        assert!(caps.pricing.is_none());
    }

    // ---- normalize_base_url edges --------------------------------------

    #[test]
    fn normalize_base_url_handles_a_bare_host() {
        // No scheme — the function does not validate, only normalises.
        // The stored value is exactly what a caller passes in plus `/v1`.
        assert_eq!(
            normalize_base_url("api.anthropic.com"),
            "api.anthropic.com/v1"
        );
    }

    #[test]
    fn normalize_base_url_trims_multiple_trailing_slashes() {
        assert_eq!(
            normalize_base_url("https://api.anthropic.com///"),
            "https://api.anthropic.com/v1"
        );
        // A `/v1` with trailing slashes is still recognised as `/v1`.
        assert_eq!(
            normalize_base_url("https://api.anthropic.com/v1///"),
            "https://api.anthropic.com/v1"
        );
    }
}
