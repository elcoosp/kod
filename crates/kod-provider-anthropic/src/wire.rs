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
use serde_json::{json, Value};

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
    let last_cacheable = prompt
        .segments
        .iter()
        .rposition(|s| s.cacheable);

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
        if let Some(last) = out.last_mut() {
            if last["role"] == json!(role) {
                if let Some(arr) = last["content"].as_array_mut() {
                    // Existing entry's content is always an array by the
                    // time we get here (the initializer for the user
                    // branch is a single object, so normalise).
                    if block_content.is_array() {
                        for b in block_content.as_array().unwrap() {
                            arr.push(b.clone());
                        }
                    } else {
                        arr.push(block_content);
                    }
                }
                continue;
            }
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
pub fn parse_sse_line(state: &mut AnthropicStreamState, line: &str) -> Vec<kod_provider::StreamChunk> {
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
            let out = v
                .get("usage")
                .and_then(|u| u.get("output_tokens"))
                .and_then(|n| n.as_u64())
                .unwrap_or(0) as usize;
            if out == 0 && state.input_tokens == 0 {
                return Vec::new();
            }
            vec![StreamChunk::Usage(kod_provider::TokenUsage {
                prompt_tokens: state.input_tokens,
                completion_tokens: out,
                total_tokens: state.input_tokens + out,
            })]
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
    use kod_provider::request::{ModelRef, SystemSegment};
    use kod_provider::GenerationOptions;
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
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[1]["type"], "tool_result");
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
        assert_eq!(body["system"][1]["cache_control"]["type"], "ephemeral");
        assert_eq!(body["messages"][0]["role"], "user");
    }

    #[test]
    fn tools_array_uses_input_schema() {
        use kod_types::{ToolCategory, ToolId, ToolPermissions};
        let tool = ToolDefinition {
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
        assert!(v[0].get("parameters").is_none(), "Anthropic uses input_schema, not parameters");
    }
}
