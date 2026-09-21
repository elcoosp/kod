//! Native Anthropic Messages API wire mapping.
//!
//! # Why a module
//!
//! `adk-model`'s Anthropic client takes a single `system: String` — it
//! flattens the system prompt before the wire call, which is fatal to
//! prompt caching: Anthropic's `cache_control: {"type": "ephemeral"}`
//! is placed on a *block* inside a system array, not on a top-level
//! string. The design (AD-16, A5b) needs the structured form on the
//! wire, so the wrapper builds the request body itself for the
//! `complete()` path and leaves the legacy text path on `adk-model`.
//!
//! # Contract
//!
//! `build_messages_body` is a pure function: `CompletionRequest` in,
//! `serde_json::Value` out. Every behaviour that matters for the wire
//! protocol is asserted in the unit tests below — the actual POST that
//! `AnthropicProvider::complete()` does is a thin wrapper around it.
//!
//! # Cache breakpoints
//!
//! The design places `cache_control` on the **last cacheable** system
//! segment. Anthropic's contract: the ephemeral marker caches
//! everything from the start of the request up to and including the
//! marked block. A second cacheable segment after the marked one would
//! be uncached; a volatile segment before the marked one would be
//! cached, which is wrong. The "last cacheable" rule puts the
//! breakpoint at the end of the invariant prefix — exactly what the
//! design's golden-prefix invariant protects.
//!
//! # Message merging
//!
//! The Messages API requires alternating user/assistant turns.
//! `ChatMessage` can produce two consecutive `user` entries — a real
//! user turn followed by a `Tool` result, which is a user-role
//! `tool_result` block on the wire. The converter merges them into one
//! entry so the API accepts the request.

use kod_provider::request::{CompletionRequest, SystemPrompt};
use kod_types::{ChatMessage, MessageRole, ToolDefinition};
use serde_json::{Value, json};

/// Build the JSON body for `POST /v1/messages`.
pub fn build_messages_body(req: &CompletionRequest) -> Value {
    let mut body = json!({
        "model": req.model.model,
        "max_tokens": req.options.max_tokens.unwrap_or(4096),
        "system": system_blocks(&req.system),
        "messages": messages_array(&req.messages),
    });

    if let Some(t) = req.options.temperature {
        body["temperature"] = json!(t);
    }
    if let Some(p) = req.options.top_p {
        body["top_p"] = json!(p);
    }
    if !req.options.stop_sequences.is_empty() {
        body["stop_sequences"] = json!(req.options.stop_sequences);
    }
    if !req.tools.is_empty() {
        body["tools"] = tools_array(&req.tools);
    }
    body
}

/// Convert `SystemPrompt` to the Anthropic `system` array, placing a
/// cache breakpoint on the last cacheable segment.
///
/// Empty prompt → empty array (the API tolerates a missing system
/// field; an empty array is equivalent and simpler to assert in tests).
pub fn system_blocks(prompt: &SystemPrompt) -> Value {
    if prompt.segments.is_empty() {
        return json!([]);
    }
    let last_cacheable = prompt.segments.iter().rposition(|s| s.cacheable);

    let mut blocks: Vec<Value> = Vec::with_capacity(prompt.segments.len());
    for (i, seg) in prompt.segments.iter().enumerate() {
        let mut block = json!({
            "type": "text",
            "text": seg.text,
        });
        if Some(i) == last_cacheable {
            block["cache_control"] = json!({"type": "ephemeral"});
        }
        blocks.push(block);
    }
    json!(blocks)
}

/// Convert `ChatMessage` list to the Anthropic `messages` array.
///
/// Merges consecutive same-role entries (the API requires alternating
/// turns). A `ChatMessage::Tool` becomes a `user` turn with a
/// `tool_result` content block. A `ChatMessage::Assistant` with
/// `tool_calls` becomes an assistant turn with `tool_use` blocks
/// alongside any text.
///
/// System messages are silently dropped: on the Anthropic wire the
/// system prompt is the top-level `system` field, not a message. A
/// `ChatMessage::System` in the transcript would be a caller bug; the
/// converter drops it rather than emit an invalid request.
///
/// `Role::Agent(_)` is treated like `Assistant` — the transcript never
/// contains them (they are display-only in the TUI), and mapping them
/// to assistant is the least wrong default if one ever leaks in.
pub fn messages_array(messages: &[ChatMessage]) -> Value {
    let mut out: Vec<Value> = Vec::new();
    for m in messages {
        let (role, block) = match &m.role {
            MessageRole::System => continue,
            MessageRole::User => ("user", json!({"type": "text", "text": m.content})),
            MessageRole::Assistant | MessageRole::Agent(_) => {
                // Assistant message: text block (if any) + tool_use blocks.
                let mut blocks: Vec<Value> = Vec::new();
                if !m.content.is_empty() {
                    blocks.push(json!({"type": "text", "text": m.content}));
                }
                for call in &m.tool_calls {
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": call.id.clone().unwrap_or_default(),
                        "name": call.tool_name,
                        "input": call.arguments,
                    }));
                }
                if blocks.is_empty() {
                    // A truly empty assistant turn — the API rejects an
                    // empty content array, so an empty text block is the
                    // least-wrong placeholder. A caller that produced
                    // this has a bug upstream; the wire stays valid.
                    blocks.push(json!({"type": "text", "text": ""}));
                }
                ("assistant", json!(blocks))
            }
            MessageRole::Tool => {
                // Tool result: user-role with a `tool_result` block.
                // `tool_call_id` is required by the API; a missing one
                // means the caller dropped the link. Fall back to an
                // empty string — the API rejects it, but the local
                // error is closer to the bug than a crash here.
                let id = m.tool_call_id.clone().unwrap_or_default();
                (
                    "user",
                    json!([{
                        "type": "tool_result",
                        "tool_use_id": id,
                        "content": m.content,
                    }]),
                )
            }
        };

        // Merge into the previous entry when the role matches.
        let block_content = block;
        if let Some(last) = out.last_mut()
            && last["role"] == json!(role)
        {
            if let Some(arr) = last["content"].as_array_mut() {
                // H-P9: Anthropic requires `tool_result` blocks at
                // the *start* of a user turn's content array. The
                // pre-fix shape simply appended, so a merged
                // `[text, tool_result]` (the note / steering flows
                // produce this) was rejected by the API. Normalize
                // the array so every `tool_result` precedes every
                // `text` block.
                let mut new_blocks: Vec<Value> = Vec::new();
                if block_content.is_array() {
                    for b in block_content.as_array().unwrap() {
                        new_blocks.push(b.clone());
                    }
                } else {
                    new_blocks.push(block_content);
                }
                // Split the existing content and the new blocks into
                // tool_result-first, then text-last.
                let mut tool_results: Vec<Value> = Vec::new();
                let mut others: Vec<Value> = Vec::new();
                for existing in arr.drain(..) {
                    if existing.get("type").and_then(|t| t.as_str()) == Some("tool_result") {
                        tool_results.push(existing);
                    } else {
                        others.push(existing);
                    }
                }
                for b in new_blocks {
                    if b.get("type").and_then(|t| t.as_str()) == Some("tool_result") {
                        tool_results.push(b);
                    } else {
                        others.push(b);
                    }
                }
                for b in tool_results {
                    arr.push(b);
                }
                for b in others {
                    arr.push(b);
                }
            }
            continue;
        }

        // Normalise a single object into an array for consistency with
        // the merge logic above.
        let content = if block_content.is_array() {
            block_content
        } else {
            json!([block_content])
        };
        out.push(json!({"role": role, "content": content}));
    }
    json!(out)
}

/// `build_messages_body` with `"stream": true` set.
///
/// Kept as a distinct function rather than a boolean parameter on
/// `build_messages_body` so a test can assert the collected body has
/// no `stream` key — that is the shape Anthropic's non-streaming
/// endpoint expects, and silently sending `stream: false` would be
/// accepted but logged differently by some proxies.
pub fn build_streaming_body(req: &CompletionRequest) -> Value {
    let mut body = build_messages_body(req);
    body["stream"] = json!(true);
    body
}

/// Convert kod tool definitions to Anthropic's `tools` array shape.
///
/// Anthropic uses `input_schema` where OpenAI uses `parameters`. The
/// description is passed through; a missing description becomes an
/// empty string so the tool block is well-formed.
pub fn tools_array(tools: &[ToolDefinition]) -> Value {
    let arr: Vec<Value> = tools
        .iter()
        .map(|t| {
            json!({
                "name": t.name,
                "description": t.description,
                "input_schema": t.parameters_schema,
            })
        })
        .collect();
    json!(arr)
}

/// The Anthropic streaming protocol is event-typed, not
/// content-typed: each `event:` line names the shape of the following
/// `data:` payload. The set of events we care about:
///
/// - `content_block_start`: opens either a text block or a tool_use
///   block. Carries the tool name and id for a tool_use block, which
///   is where a `ToolCallStart` chunk comes from.
/// - `content_block_delta`: an incremental update. `text_delta`
///   carries a text fragment; `input_json_delta` carries a fragment of
///   the tool's arguments JSON.
/// - `message_delta`: end-of-message metadata. The `usage.output_tokens`
///   figure arrives here (input tokens were on `message_start`).
/// - `message_stop`: terminator.
///
/// The parser is a plain `&str -> Vec<StreamChunk>` (plus a
/// `pending_stop` flag for the terminator). The provider drives it from
/// the SSE byte stream and forwards what it returns.
#[derive(Default)]
pub struct AnthropicStreamState {
    /// Set once the SSE stream has emitted `message_stop`. The provider
    /// emits a final `StreamChunk::Done` when this is set and the byte
    /// stream ends, even if no further lines arrive.
    pub finished: bool,
    /// Accumulated input tokens from `message_start`.
    pub input_tokens: usize,
    /// Blocks observed so far. Used to correlate `content_block_delta`
    /// events (which carry only an index) with the tool name/id
    /// recorded at `content_block_start`.
    pub blocks: std::collections::HashMap<usize, BlockInfo>,
}

/// Per-block state carried across events for one response.
#[derive(Default)]
pub struct BlockInfo {
    /// `text` or `tool_use`. Unknown types are ignored.
    pub kind: String,
    pub tool_id: Option<String>,
    pub tool_name: Option<String>,
}

/// Feed one SSE line (already trimmed of `\n`) into the state machine.
/// Returns the chunks the line produced. Lines that are blank, comments,
/// or events we do not consume return an empty vec.
pub fn parse_sse_line(
    state: &mut AnthropicStreamState,
    line: &str,
) -> Vec<kod_provider::StreamChunk> {
    use kod_provider::StreamChunk;

    // The event name line is informational; we key on the JSON `type`
    // field inside the data payload, which every provider emits and
    // which is what the streaming spec pins. This lets the parser
    // ignore `event:` entirely without losing information.
    let Some(rest) = line.strip_prefix("data:") else {
        return Vec::new();
    };
    let payload = rest.trim_start();
    if payload == "[DONE]" || payload.is_empty() {
        return Vec::new();
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(payload) else {
        // A malformed frame is dropped: the byte stream is
        // non-fatal and the next frame may be well-formed. A real
        // protocol error would produce zero readable frames, and the
        // provider's timeout catches that.
        return Vec::new();
    };

    match v.get("type").and_then(|t| t.as_str()) {
        Some("message_start") => {
            if let Some(n) = v
                .get("message")
                .and_then(|m| m.get("usage"))
                .and_then(|u| u.get("input_tokens"))
                .and_then(|n| n.as_u64())
            {
                state.input_tokens = n as usize;
            }
            Vec::new()
        }
        Some("content_block_start") => {
            let index = v.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
            let block = v.get("content_block").cloned().unwrap_or_default();
            let kind = block
                .get("type")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string();
            let info = BlockInfo {
                kind: kind.clone(),
                tool_id: block.get("id").and_then(|i| i.as_str()).map(String::from),
                tool_name: block.get("name").and_then(|n| n.as_str()).map(String::from),
            };
            state.blocks.insert(index, info);
            if kind == "tool_use" {
                let entry = state.blocks.get(&index).unwrap();
                let name = entry.tool_name.clone().unwrap_or_default();
                if name.is_empty() {
                    return Vec::new();
                }
                vec![StreamChunk::ToolCallStart {
                    index,
                    id: entry.tool_id.clone(),
                    name,
                }]
            } else {
                Vec::new()
            }
        }
        Some("content_block_delta") => {
            let index = v.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
            let delta = v.get("delta").cloned().unwrap_or_default();
            match delta.get("type").and_then(|t| t.as_str()) {
                Some("text_delta") => {
                    let text = delta
                        .get("text")
                        .and_then(|t| t.as_str())
                        .unwrap_or("")
                        .to_string();
                    if text.is_empty() {
                        Vec::new()
                    } else {
                        vec![StreamChunk::Text(text)]
                    }
                }
                Some("input_json_delta") => {
                    let partial = delta
                        .get("partial_json")
                        .and_then(|p| p.as_str())
                        .unwrap_or("")
                        .to_string();
                    if partial.is_empty() {
                        Vec::new()
                    } else {
                        vec![StreamChunk::ToolCallDelta {
                            index,
                            arguments: partial,
                        }]
                    }
                }
                _ => Vec::new(),
            }
        }
        Some("message_delta") => {
            // Anthropic reports the final output token count here; the
            // input count came from `message_start`. Emit a combined
            // Usage once, when the delta is seen.
            //
            // H-P6: `delta.stop_reason` ("max_tokens", "refusal",
            // "pause_turn", "tool_use", "end_turn") is surfaced as a
            // `StopReason` chunk. Without it, a response truncated
            // mid tool-JSON was indistinguishable from a complete
            // one — the engine could only see the stream end.
            let mut chunks = Vec::new();
            if let Some(reason) = v
                .get("delta")
                .and_then(|d| d.get("stop_reason"))
                .and_then(|s| s.as_str())
                && !reason.is_empty()
            {
                chunks.push(StreamChunk::StopReason(reason.to_string()));
            }
            let out = v
                .get("usage")
                .and_then(|u| u.get("output_tokens"))
                .and_then(|n| n.as_u64())
                .unwrap_or(0) as usize;
            if out > 0 || state.input_tokens > 0 {
                chunks.push(StreamChunk::Usage(kod_provider::TokenUsage {
                    prompt_tokens: state.input_tokens,
                    completion_tokens: out,
                    total_tokens: state.input_tokens + out,
                }));
            }
            chunks
        }
        Some("message_stop") => {
            state.finished = true;
            vec![StreamChunk::Done]
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kod_provider::GenerationOptions;
    use kod_provider::request::{ModelRef, SystemSegment};
    use kod_types::{MessageId, ToolCall};
    use time::OffsetDateTime;

    fn user(content: &str) -> ChatMessage {
        ChatMessage::text(
            MessageId::new(),
            MessageRole::User,
            content,
            OffsetDateTime::now_utc(),
        )
    }

    fn assistant(content: &str) -> ChatMessage {
        ChatMessage::text(
            MessageId::new(),
            MessageRole::Assistant,
            content,
            OffsetDateTime::now_utc(),
        )
    }

    fn request_with(prompt: SystemPrompt, messages: Vec<ChatMessage>) -> CompletionRequest {
        let mut req = CompletionRequest::new(ModelRef::new("anthropic", "claude-sonnet-4-5"));
        req.system = prompt;
        req.messages = messages;
        req.options = GenerationOptions {
            max_tokens: Some(1024),
            ..Default::default()
        };
        req
    }

    #[test]
    fn sse_message_start_sets_input_tokens() {
        let mut st = super::AnthropicStreamState::default();
        let line = "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\",\"role\":\"assistant\",\"content\":[],\"usage\":{\"input_tokens\":17,\"output_tokens\":0}}}";
        let chunks = super::parse_sse_line(&mut st, line);
        assert!(chunks.is_empty(), "message_start yields no chunks");
        assert_eq!(
            st.input_tokens, 17,
            "input_tokens must be captured for the later Usage"
        );
    }

    #[test]
    fn sse_content_block_start_text_yields_nothing() {
        let mut st = super::AnthropicStreamState::default();
        let line = "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}";
        let chunks = super::parse_sse_line(&mut st, line);
        assert!(chunks.is_empty(), "a text block start yields no chunk");
    }

    #[test]
    fn sse_content_block_start_tool_use_yields_start() {
        let mut st = super::AnthropicStreamState::default();
        let line = "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"read_file\",\"input\":{}}}";
        let chunks = super::parse_sse_line(&mut st, line);
        assert_eq!(chunks.len(), 1);
        match &chunks[0] {
            kod_provider::StreamChunk::ToolCallStart { index, id, name } => {
                assert_eq!(*index, 0);
                assert_eq!(id.as_deref(), Some("toolu_1"));
                assert_eq!(name, "read_file");
            }
            other => panic!("expected ToolCallStart, got {other:?}"),
        }
    }

    #[test]
    fn sse_text_delta_yields_text_chunk() {
        let mut st = super::AnthropicStreamState::default();
        let line = "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}";
        let chunks = super::parse_sse_line(&mut st, line);
        assert_eq!(chunks.len(), 1);
        match &chunks[0] {
            kod_provider::StreamChunk::Text(t) => assert_eq!(t, "hello"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn sse_input_json_delta_yields_tool_call_delta() {
        let mut st = super::AnthropicStreamState::default();
        // Prime a tool block so the index is known.
        let start = "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"read_file\",\"input\":{}}}";
        let _ = super::parse_sse_line(&mut st, start);
        let line = "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\\\"a.rs\\\"}\"}}";
        let chunks = super::parse_sse_line(&mut st, line);
        assert_eq!(chunks.len(), 1);
        match &chunks[0] {
            kod_provider::StreamChunk::ToolCallDelta { index, arguments } => {
                assert_eq!(*index, 0);
                assert!(arguments.contains("a.rs"), "partial JSON must be captured");
            }
            other => panic!("expected ToolCallDelta, got {other:?}"),
        }
    }

    #[test]
    fn sse_message_delta_yields_usage() {
        let mut st = super::AnthropicStreamState::default();
        // Prime the input token count.
        let start = "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\",\"role\":\"assistant\",\"content\":[],\"usage\":{\"input_tokens\":10,\"output_tokens\":0}}}";
        let _ = super::parse_sse_line(&mut st, start);
        let line = "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":4}}";
        let chunks = super::parse_sse_line(&mut st, line);
        // H-P6: the delta now yields a StopReason chunk *and* a Usage
        // chunk. Find the Usage; ignore the reason.
        let usage = chunks
            .iter()
            .find_map(|c| match c {
                kod_provider::StreamChunk::Usage(u) => Some(u),
                _ => None,
            })
            .expect("Usage must be emitted");
        assert_eq!(usage.prompt_tokens, 10);
        assert_eq!(usage.completion_tokens, 4);
        assert_eq!(usage.total_tokens, 14);
    }

    #[test]
    fn sse_message_stop_yields_done() {
        let mut st = super::AnthropicStreamState::default();
        let line = "data: {\"type\":\"message_stop\"}";
        let chunks = super::parse_sse_line(&mut st, line);
        assert_eq!(chunks.len(), 1);
        assert!(matches!(chunks[0], kod_provider::StreamChunk::Done));
        assert!(st.finished);
    }

    #[test]
    fn sse_unknown_event_is_ignored() {
        let mut st = super::AnthropicStreamState::default();
        let line = "data: {\"type\":\"something.new\",\"value\":42}";
        assert!(super::parse_sse_line(&mut st, line).is_empty());
    }

    #[test]
    fn sse_malformed_json_is_ignored() {
        let mut st = super::AnthropicStreamState::default();
        assert!(super::parse_sse_line(&mut st, "data: not json").is_empty());
    }

    #[test]
    fn sse_event_line_is_ignored() {
        // The `event:` line is informational — the parser keys on the
        // JSON `type` field inside the `data:` payload.
        let mut st = super::AnthropicStreamState::default();
        assert!(super::parse_sse_line(&mut st, "event: message_start").is_empty());
    }

    #[test]
    fn sse_blank_line_is_ignored() {
        let mut st = super::AnthropicStreamState::default();
        assert!(super::parse_sse_line(&mut st, "").is_empty());
    }

    #[test]
    fn sse_full_sequence_produces_expected_chunks() {
        // A realistic sequence: text + tool use, end to end.
        let mut st = super::AnthropicStreamState::default();
        let stream = [
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\",\"role\":\"assistant\",\"content\":[],\"usage\":{\"input_tokens\":5,\"output_tokens\":0}}}",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Let me check.\"}}",
            "data: {\"type\":\"content_block_stop\",\"index\":0}",
            "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_2\",\"name\":\"read_file\",\"input\":{}}}",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\\\"src/main.rs\\\"}\"}}",
            "data: {\"type\":\"content_block_stop\",\"index\":1}",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":12}}",
            "data: {\"type\":\"message_stop\"}",
        ];
        let mut texts = String::new();
        let mut starts = 0usize;
        let mut deltas = 0usize;
        let mut usage: Option<kod_provider::TokenUsage> = None;
        let mut done = false;
        for line in stream {
            for chunk in super::parse_sse_line(&mut st, line) {
                match chunk {
                    kod_provider::StreamChunk::Text(t) => texts.push_str(&t),
                    kod_provider::StreamChunk::ToolCallStart { .. } => starts += 1,
                    kod_provider::StreamChunk::ToolCallDelta { .. } => deltas += 1,
                    kod_provider::StreamChunk::Usage(u) => usage = Some(u),
                    kod_provider::StreamChunk::StopReason(_) => {}
                    kod_provider::StreamChunk::Done => done = true,
                }
            }
        }
        assert_eq!(texts, "Let me check.");
        assert_eq!(starts, 1, "one tool call started");
        assert_eq!(deltas, 1, "one argument delta seen");
        assert!(done, "sequence terminated with Done");
        let u = usage.expect("usage must be emitted");
        assert_eq!(u.prompt_tokens, 5);
        assert_eq!(u.completion_tokens, 12);
    }

    #[test]
    fn system_prompt_places_cache_control_on_last_cacheable_segment() {
        let prompt = SystemPrompt {
            segments: vec![
                SystemSegment {
                    text: "identity".into(),
                    cacheable: true,
                },
                SystemSegment {
                    text: "repomap".into(),
                    cacheable: true,
                },
                SystemSegment {
                    text: "environment".into(),
                    cacheable: false,
                },
            ],
        };
        let blocks = system_blocks(&prompt);
        let arr = blocks.as_array().unwrap();
        assert_eq!(arr.len(), 3);
        // First cacheable: no marker (it is covered by the next one).
        assert!(arr[0].get("cache_control").is_none());
        assert_eq!(arr[0]["text"], "identity");
        // Last cacheable: marked. This is the design's rule.
        assert_eq!(arr[1]["cache_control"]["type"], "ephemeral");
        assert_eq!(arr[1]["text"], "repomap");
        // Volatile: not marked.
        assert!(arr[2].get("cache_control").is_none());
        assert_eq!(arr[2]["text"], "environment");
    }

    #[test]
    fn system_prompt_with_only_volatile_has_no_marker() {
        let prompt = SystemPrompt {
            segments: vec![SystemSegment {
                text: "only volatile".into(),
                cacheable: false,
            }],
        };
        let arr = system_blocks(&prompt);
        assert!(arr.as_array().unwrap()[0].get("cache_control").is_none());
    }

    #[test]
    fn empty_system_prompt_yields_empty_array() {
        let arr = system_blocks(&SystemPrompt::default());
        assert!(arr.as_array().unwrap().is_empty());
    }

    #[test]
    fn user_message_becomes_single_text_block() {
        let msgs = vec![user("hello")];
        let arr = messages_array(&msgs);
        let v = arr.as_array().unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0]["role"], "user");
        assert_eq!(v[0]["content"][0]["type"], "text");
        assert_eq!(v[0]["content"][0]["text"], "hello");
    }

    #[test]
    fn assistant_with_tool_calls_becomes_tool_use_blocks() {
        let mut a = assistant("let me look that up");
        a.tool_calls.push(ToolCall {
            id: Some("call_1".into()),
            tool_name: "read_file".into(),
            arguments: serde_json::json!({"path": "src/main.rs"}),
        });
        let arr = messages_array(&[a]);
        let v = arr.as_array().unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0]["role"], "assistant");
        let content = v[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "let me look that up");
        assert_eq!(content[1]["type"], "tool_use");
        assert_eq!(content[1]["id"], "call_1");
        assert_eq!(content[1]["name"], "read_file");
        assert_eq!(content[1]["input"]["path"], "src/main.rs");
    }

    #[test]
    fn tool_message_becomes_user_role_tool_result() {
        let mut tool_msg = ChatMessage::text(
            MessageId::new(),
            MessageRole::Tool,
            "file content here",
            OffsetDateTime::now_utc(),
        );
        tool_msg.tool_call_id = Some("call_1".into());
        let arr = messages_array(&[assistant("calling"), tool_msg]);
        let v = arr.as_array().unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[1]["role"], "user");
        let content = v[1]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "tool_result");
        assert_eq!(content[0]["tool_use_id"], "call_1");
        assert_eq!(content[0]["content"], "file content here");
    }

    #[test]
    fn consecutive_user_messages_are_merged() {
        // A real user turn followed by a tool result would produce two
        // user entries after conversion. The API requires alternating
        // roles, so they must be merged.
        let mut tool_msg = ChatMessage::text(
            MessageId::new(),
            MessageRole::Tool,
            "result",
            OffsetDateTime::now_utc(),
        );
        tool_msg.tool_call_id = Some("c1".into());
        let msgs = vec![user("first"), tool_msg];
        let arr = messages_array(&msgs);
        let v = arr.as_array().unwrap();
        assert_eq!(v.len(), 1, "consecutive user messages must merge");
        assert_eq!(v[0]["role"], "user");
        let content = v[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        // H-P9: `tool_result` blocks come first. The API rejects
        // `[text, tool_result]`.
        assert_eq!(content[0]["type"], "tool_result");
        assert_eq!(content[1]["type"], "text");
    }

    #[test]
    fn system_messages_are_dropped() {
        let sys = ChatMessage::text(
            MessageId::new(),
            MessageRole::System,
            "should be dropped",
            OffsetDateTime::now_utc(),
        );
        let arr = messages_array(&[sys, user("hi")]);
        let v = arr.as_array().unwrap();
        assert_eq!(v.len(), 1, "System messages do not appear on the wire");
        assert_eq!(v[0]["role"], "user");
    }

    #[test]
    fn full_body_has_expected_fields() {
        let prompt = SystemPrompt {
            segments: vec![
                SystemSegment {
                    text: "id".into(),
                    cacheable: true,
                },
                SystemSegment {
                    text: "vol".into(),
                    cacheable: false,
                },
            ],
        };
        let req = request_with(prompt, vec![user("hi")]);
        let body = build_messages_body(&req);
        assert_eq!(body["model"], "claude-sonnet-4-5");
        assert_eq!(body["max_tokens"], 1024);
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(body["messages"][0]["role"], "user");
    }

    #[test]
    fn tools_array_uses_input_schema() {
        use kod_types::{ToolCategory, ToolId, ToolPermissions};
        let tool = ToolDefinition {
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
        };
        let arr = tools_array(&[tool]);
        let v = arr.as_array().unwrap();
        assert_eq!(v[0]["name"], "read_file");
        assert_eq!(v[0]["description"], "read a file");
        assert_eq!(v[0]["input_schema"]["type"], "object");
        assert!(
            v[0].get("parameters").is_none(),
            "Anthropic uses input_schema, not parameters"
        );
    }
}

/// Coverage for the request builders' optional fields and the
/// tolerance branches in `messages_array` that the existing tests do
/// not reach. The SSE parser gets its own module elsewhere; this one
/// is only about what `build_messages_body` and its helpers emit.
#[cfg(test)]
mod coverage_wire_builders {
    use super::*;
    use kod_provider::{ModelRef, SystemPrompt};
    use kod_types::{MessageId, MessageRole, ToolCall, ToolDefinition};
    use serde_json::json;
    use time::OffsetDateTime;

    fn ts() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(0).unwrap()
    }

    fn assistant_with_calls(text: &str, calls: Vec<ToolCall>) -> ChatMessage {
        let mut m = ChatMessage::text(MessageId::new(), MessageRole::Assistant, text, ts());
        m.tool_calls = calls;
        m
    }

    fn tool_message(content: &str, id: Option<&str>) -> ChatMessage {
        let mut m = ChatMessage::text(MessageId::new(), MessageRole::Tool, content, ts());
        m.tool_call_id = id.map(String::from);
        m
    }

    fn user_message(content: &str) -> ChatMessage {
        ChatMessage::text(MessageId::new(), MessageRole::User, content, ts())
    }

    fn a_tool_call(id: Option<&str>, name: &str) -> ToolCall {
        ToolCall {
            id: id.map(String::from),
            tool_name: name.to_string(),
            arguments: json!({"path": "src/lib.rs"}),
        }
    }

    fn a_tool_def(name: &str) -> ToolDefinition {
        ToolDefinition {
            trust_level: kod_types::trust::TrustLevel::default(),
            id: kod_types::ToolId::new(),
            name: name.to_string(),
            description: format!("does {name}"),
            category: kod_types::ToolCategory::Code,
            parameters_schema: json!({"type": "object"}),
            permissions: kod_types::ToolPermissions::default(),
        }
    }

    fn base_req() -> CompletionRequest {
        CompletionRequest::new(ModelRef::new("anthropic", "claude-sonnet-4-5"))
    }

    // ---- build_messages_body optional fields --------------------------

    #[test]
    fn body_defaults_max_tokens_to_4096_when_unset() {
        let req = base_req();
        let body = build_messages_body(&req);
        assert_eq!(body["max_tokens"], 4096);
    }

    #[test]
    fn body_carries_max_tokens_when_set() {
        let mut req = base_req();
        req.options.max_tokens = Some(1024);
        let body = build_messages_body(&req);
        assert_eq!(body["max_tokens"], 1024);
    }

    #[test]
    fn body_carries_the_model_from_the_model_ref() {
        let req = CompletionRequest::new(ModelRef::new("endpoint-ignored", "the-model"));
        let body = build_messages_body(&req);
        assert_eq!(body["model"], "the-model");
    }

    #[test]
    fn body_omits_temperature_when_unset() {
        // Anthropic's API will use its own default. Sending an
        // explicit null or a wrong default would override that.
        let req = base_req();
        let body = build_messages_body(&req);
        assert!(
            body.get("temperature").is_none(),
            "unset temperature must be omitted, got: {body}",
        );
    }

    #[test]
    fn body_carries_temperature_when_set() {
        let mut req = base_req();
        req.options.temperature = Some(0.7);
        let body = build_messages_body(&req);
        assert_eq!(body["temperature"], 0.7f32 as f64);
    }

    #[test]
    fn body_omits_top_p_when_unset() {
        let req = base_req();
        let body = build_messages_body(&req);
        assert!(body.get("top_p").is_none());
    }

    #[test]
    fn body_carries_top_p_when_set() {
        let mut req = base_req();
        req.options.top_p = Some(0.9);
        let body = build_messages_body(&req);
        assert_eq!(body["top_p"], 0.9f32 as f64);
    }

    #[test]
    fn body_omits_stop_sequences_when_empty() {
        let req = base_req();
        let body = build_messages_body(&req);
        assert!(body.get("stop_sequences").is_none());
    }

    #[test]
    fn body_carries_stop_sequences_when_present() {
        let mut req = base_req();
        req.options.stop_sequences = vec!["STOP".to_string(), "\n\n".to_string()];
        let body = build_messages_body(&req);
        let arr = body["stop_sequences"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0], "STOP");
        assert_eq!(arr[1], "\n\n");
    }

    #[test]
    fn body_omits_tools_when_empty() {
        let req = base_req();
        let body = build_messages_body(&req);
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn body_includes_tools_when_present() {
        let mut req = base_req();
        req.tools = vec![a_tool_def("read_file"), a_tool_def("grep")];
        let body = build_messages_body(&req);
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0]["name"], "read_file");
        assert_eq!(tools[1]["name"], "grep");
        // Anthropic uses `input_schema`; the shape is the tool
        // definition's `parameters_schema` verbatim.
        assert_eq!(tools[0]["input_schema"]["type"], "object");
    }

    #[test]
    fn body_never_emits_the_stream_key() {
        // The non-streaming endpoint rejects a `stream: false` on some
        // proxies; the streaming builder adds it explicitly. The two
        // bodies must stay distinguishable.
        let req = base_req();
        let body = build_messages_body(&req);
        assert!(
            body.get("stream").is_none(),
            "non-streaming body must not carry `stream`: {body}",
        );
    }

    // ---- build_streaming_body ----------------------------------------

    #[test]
    fn streaming_body_sets_stream_true() {
        let req = base_req();
        let body = build_streaming_body(&req);
        assert_eq!(body["stream"], true);
    }

    #[test]
    fn streaming_body_has_the_same_non_stream_fields_as_the_plain_body() {
        // A regression that built the streaming body from scratch
        // (instead of delegating to `build_messages_body`) would
        // silently drop an option. Compare a few representative keys
        // rather than the whole object so the test is not brittle
        // against unrelated additions.
        let mut req = base_req();
        req.options.max_tokens = Some(2048);
        req.options.temperature = Some(0.5);
        let plain = build_messages_body(&req);
        let stream = build_streaming_body(&req);
        for key in ["model", "max_tokens", "temperature", "system", "messages"] {
            assert_eq!(stream[key], plain[key], "streaming body dropped `{key}`");
        }
        assert!(plain.get("stream").is_none());
        assert_eq!(stream["stream"], true);
    }

    // ---- messages_array tolerance branches ----------------------------

    #[test]
    fn empty_assistant_message_becomes_an_empty_text_block() {
        // A truly empty assistant turn — the API rejects an empty
        // content array, so the converter emits one empty text block.
        // This is the least-wrong placeholder for a caller bug
        // upstream.
        let msgs = vec![assistant_with_calls("", vec![])];
        let arr = messages_array(&msgs);
        let first = &arr[0];
        assert_eq!(first["role"], "assistant");
        let content = first["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "");
    }

    #[test]
    fn assistant_with_text_and_tool_calls_emits_text_first() {
        let msgs = vec![assistant_with_calls(
            "thinking…",
            vec![a_tool_call(Some("c1"), "read_file")],
        )];
        let arr = messages_array(&msgs);
        let content = arr[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2, "text + one tool_use block");
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "thinking…");
        assert_eq!(content[1]["type"], "tool_use");
        assert_eq!(content[1]["id"], "c1");
        assert_eq!(content[1]["name"], "read_file");
    }

    #[test]
    fn assistant_tool_call_with_no_id_becomes_an_empty_id_string() {
        // `unwrap_or_default()` on the call id. A missing id is a
        // caller bug — the server will reject it — but the converter
        // must not panic.
        let msgs = vec![assistant_with_calls("", vec![a_tool_call(None, "grep")])];
        let arr = messages_array(&msgs);
        let content = arr[0]["content"].as_array().unwrap();
        // Only the tool_use block (no empty text prefix since the
        // call exists).
        let tool_use = content.iter().find(|b| b["type"] == "tool_use").unwrap();
        assert_eq!(tool_use["id"], "");
    }

    #[test]
    fn agent_role_is_treated_as_assistant() {
        // The transcript should never contain an Agent message, but
        // if one leaks the converter maps it to assistant rather
        // than emit an invalid request.
        let msgs = vec![ChatMessage::text(
            MessageId::new(),
            MessageRole::Agent(kod_types::AgentId::new()),
            "agent says hi",
            ts(),
        )];
        let arr = messages_array(&msgs);
        assert_eq!(arr[0]["role"], "assistant");
        assert_eq!(arr[0]["content"][0]["text"], "agent says hi");
    }

    #[test]
    fn tool_message_without_a_call_id_uses_an_empty_string() {
        // Same tolerance as the assistant case: the API rejects an
        // empty `tool_use_id`, but a local reject is closer to the
        // caller's bug than a panic.
        let msgs = vec![tool_message("result body", None)];
        let arr = messages_array(&msgs);
        assert_eq!(arr[0]["role"], "user");
        let block = &arr[0]["content"][0];
        assert_eq!(block["type"], "tool_result");
        assert_eq!(block["tool_use_id"], "");
        assert_eq!(block["content"], "result body");
    }

    #[test]
    fn consecutive_assistant_messages_are_merged_into_one_turn() {
        // Same merge rule as for user messages — the API requires
        // alternating roles, so two adjacent assistant turns collapse
        // into one content array.
        let msgs = vec![
            assistant_with_calls("first", vec![]),
            assistant_with_calls("second", vec![]),
        ];
        let arr = messages_array(&msgs);
        assert_eq!(arr.as_array().unwrap().len(), 1, "expected one turn");
        let content = arr[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2, "both text blocks preserved");
        assert_eq!(content[0]["text"], "first");
        assert_eq!(content[1]["text"], "second");
    }

    #[test]
    fn consecutive_tool_messages_are_merged_into_one_user_turn() {
        // A round of N tool results arrives as N consecutive `Tool`
        // messages; the API wants one user turn with N `tool_result`
        // blocks.
        let msgs = vec![
            tool_message("result a", Some("c1")),
            tool_message("result b", Some("c2")),
        ];
        let arr = messages_array(&msgs);
        assert_eq!(arr.as_array().unwrap().len(), 1);
        assert_eq!(arr[0]["role"], "user");
        let content = arr[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["tool_use_id"], "c1");
        assert_eq!(content[1]["tool_use_id"], "c2");
    }

    #[test]
    fn a_full_user_assistant_user_conversation_alternates_roles() {
        let msgs = vec![
            user_message("hi"),
            assistant_with_calls("hello", vec![]),
            user_message("bye"),
        ];
        let arr = messages_array(&msgs);
        let roles: Vec<&str> = arr
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["role"].as_str().unwrap())
            .collect();
        assert_eq!(roles, vec!["user", "assistant", "user"]);
    }

    #[test]
    fn empty_messages_array_is_an_empty_array() {
        let arr = messages_array(&[]);
        assert_eq!(arr, json!([]));
    }

    // ---- system_blocks non-last cacheable segment --------------------

    #[test]
    fn system_blocks_marks_only_the_last_cacheable_segment() {
        // Three segments, two cacheable. The marker must land on the
        // *later* cacheable one — a regression that marked the first,
        // or marked both, would make the cache split point wrong.
        let prompt = SystemPrompt::new()
            .with("first cacheable", true)
            .with("volatile", false)
            .with("last cacheable", true);
        let blocks = system_blocks(&prompt);
        let arr = blocks.as_array().unwrap();
        assert_eq!(arr.len(), 3);
        assert!(
            arr[0].get("cache_control").is_none(),
            "first must not be marked"
        );
        assert!(
            arr[1].get("cache_control").is_none(),
            "volatile must not be marked"
        );
        assert_eq!(arr[2]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn system_blocks_with_all_cacheable_marks_only_the_last() {
        let prompt = SystemPrompt::new()
            .with("a", true)
            .with("b", true)
            .with("c", true);
        let blocks = system_blocks(&prompt);
        let arr = blocks.as_array().unwrap();
        let marked: Vec<bool> = arr
            .iter()
            .map(|b| b.get("cache_control").is_some())
            .collect();
        assert_eq!(marked, vec![false, false, true]);
    }

    #[test]
    fn system_blocks_preserves_segment_order_in_the_blocks_array() {
        // The array order is the prompt's cache-prefix order; a
        // reorder would put the breakpoint after the wrong text.
        let prompt = SystemPrompt::new().with("first", true).with("second", true);
        let blocks = system_blocks(&prompt);
        assert_eq!(blocks[0]["text"], "first");
        assert_eq!(blocks[1]["text"], "second");
    }

    #[test]
    fn system_blocks_with_no_segments_is_empty_array() {
        let prompt = SystemPrompt::new();
        let blocks = system_blocks(&prompt);
        assert_eq!(blocks, json!([]));
    }

    // ---- tools_array edge cases ---------------------------------------

    #[test]
    fn tools_array_of_empty_slice_is_empty_array() {
        assert_eq!(tools_array(&[]), json!([]));
    }

    #[test]
    fn tools_array_passes_the_description_verbatim() {
        let t = a_tool_def("custom_tool");
        let arr = tools_array(&[t]);
        assert_eq!(arr[0]["description"], "does custom_tool");
    }

    #[test]
    fn sse_message_delta_yields_stop_reason() {
        // H-P6: the stop_reason field on message_delta must surface as
        // a StopReason chunk, not be silently dropped.
        let mut state = AnthropicStreamState::default();
        let line = r#"data: {"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{"output_tokens":4}}"#;
        let chunks = parse_sse_line(&mut state, line);
        assert!(
            chunks.iter().any(
                |c| matches!(c, kod_provider::StreamChunk::StopReason(r) if r == "max_tokens")
            ),
            "expected StopReason, got: {chunks:?}",
        );
    }

    #[test]
    fn sse_message_delta_without_stop_reason_yields_no_stop_chunk() {
        let mut state = AnthropicStreamState::default();
        let line = r#"data: {"type":"message_delta","delta":{},"usage":{"output_tokens":4}}"#;
        let chunks = parse_sse_line(&mut state, line);
        assert!(
            !chunks
                .iter()
                .any(|c| matches!(c, kod_provider::StreamChunk::StopReason(_))),
        );
    }

    #[test]
    fn merged_user_turn_puts_tool_result_first() {
        // H-P9: a user text message followed by a tool result must
        // serialize with the tool_result block first. The API
        // rejects `[text, tool_result]`.
        use kod_types::{ChatMessage, MessageId, MessageRole};
        use time::OffsetDateTime;
        let now = OffsetDateTime::now_utc();
        let user = ChatMessage::text(MessageId::new(), MessageRole::User, "here is a note", now);
        let mut tool = ChatMessage::text(MessageId::new(), MessageRole::Tool, "tool output", now);
        tool.tool_call_id = Some("call_1".to_string());
        let msgs = vec![user, tool];
        let arr = messages_array(&msgs);
        // One merged user turn.
        assert_eq!(arr.as_array().unwrap().len(), 1);
        let blocks = arr[0]["content"].as_array().unwrap();
        assert!(blocks.len() >= 2, "expected merged blocks, got {blocks:?}",);
        assert_eq!(
            blocks[0].get("type").and_then(|t| t.as_str()),
            Some("tool_result"),
            "tool_result must come first: {blocks:?}",
        );
    }
}
