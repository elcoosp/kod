//! Types for LLM generation.

use kod_types::ToolCall;

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TokenUsage {
    pub prompt_tokens: usize,
    pub completion_tokens: usize,
    pub total_tokens: usize,
    /// Tokens served from the provider's KV cache (Anthropic
    /// `cache_read_input_tokens`). Billed at a discounted rate — 0.1x
    /// input on Anthropic at time of writing. Zero for providers that
    /// do not report cache state.
    #[serde(default)]
    pub cache_read_tokens: usize,
    /// Tokens written to the provider's KV cache this call (Anthropic
    /// `cache_creation_input_tokens`). Billed at a premium — 1.25x
    /// input on Anthropic. Zero for providers that do not report it.
    #[serde(default)]
    pub cache_creation_tokens: usize,
}

impl TokenUsage {
    /// Input tokens billed at the *full* input rate this call.
    ///
    /// `prompt_tokens` is the total input window the provider
    /// processed (see `parse_usage` in the provider crates — Anthropic
    /// reports `input_tokens` that *excludes* the cache fields, and
    /// kod folds them into `prompt_tokens` so downstream cost math
    /// needs one number). This method recovers the portion that pays
    /// full price: everything not served from or written to a cache.
    ///
    /// Saturating: a provider that mis-reports cache fields larger
    /// than the prompt window yields 0 rather than wrapping.
    pub fn uncached_input_tokens(&self) -> usize {
        self.prompt_tokens
            .saturating_sub(self.cache_read_tokens)
            .saturating_sub(self.cache_creation_tokens)
    }

    /// Fraction of input tokens served from cache this call.
    ///
    /// Zero for a call with no input tokens or a provider that does
    /// not report cache fields — not NaN, so callers can print it
    /// without a guard.
    pub fn cache_hit_rate(&self) -> f64 {
        if self.prompt_tokens == 0 {
            0.0
        } else {
            self.cache_read_tokens as f64 / self.prompt_tokens as f64
        }
    }

    /// H-E6: sum two usage reports from the same logical turn. In an
    /// agentic loop a single user turn can issue several provider
    /// calls (one per round, plus a summary); the prompt cost of
    /// rounds 1..N−1 is exactly as real as the last one's, and the
    /// pre-fix code kept only the last round's report.
    ///
    /// Totals are summed independently — a provider that reports a
    /// `total_tokens` that is not `prompt + completion` (some cache
    /// implementations do this) keeps its own arithmetic.
    pub fn merge(&self, other: &TokenUsage) -> TokenUsage {
        TokenUsage {
            prompt_tokens: self.prompt_tokens.saturating_add(other.prompt_tokens),
            completion_tokens: self
                .completion_tokens
                .saturating_add(other.completion_tokens),
            total_tokens: self.total_tokens.saturating_add(other.total_tokens),
            cache_read_tokens: self
                .cache_read_tokens
                .saturating_add(other.cache_read_tokens),
            cache_creation_tokens: self
                .cache_creation_tokens
                .saturating_add(other.cache_creation_tokens),
        }
    }
}

#[derive(Debug, Clone)]
pub enum GenerationResponse {
    Text {
        content: String,
        usage: Option<TokenUsage>,
    },
    ToolCalls {
        calls: Vec<ToolCall>,
        usage: Option<TokenUsage>,
    },
    Mixed {
        content: String,
        calls: Vec<ToolCall>,
        usage: Option<TokenUsage>,
    },
}

impl GenerationResponse {
    pub fn usage(&self) -> Option<&TokenUsage> {
        match self {
            GenerationResponse::Text { usage, .. } => usage.as_ref(),
            GenerationResponse::ToolCalls { usage, .. } => usage.as_ref(),
            GenerationResponse::Mixed { usage, .. } => usage.as_ref(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum StreamChunk {
    Text(String),
    /// The provider has begun describing a tool call.
    ///
    /// `index` distinguishes concurrent calls within one response
    /// (OpenAI SSE numbers them; a provider that emits calls one at
    /// a time uses `index: 0`). `id` is the wire-level id when the
    /// provider reports one — `None` otherwise.
    ToolCallStart {
        index: usize,
        id: Option<String>,
        name: String,
    },
    /// A fragment of the arguments JSON for the call at `index`. A
    /// provider that emits the full arguments in one chunk sends a
    /// single delta; a provider that streams them sends several.
    ToolCallDelta {
        index: usize,
        arguments: String,
    },
    Usage(TokenUsage),
    /// H-P6: the provider's stop reason, when it reports one. The
    /// engine pre-fix had no way to learn a response was truncated
    /// mid tool-JSON — Anthropic emits `stop_reason = "max_tokens"`
    /// or `"refusal"` on `message_delta`, and it vanished into the
    /// `_` arm of the SSE parser. Distinct from `Done` (the stream
    /// is over) because the reason is a *property* of the finish,
    /// not the finish itself.
    StopReason(String),
    Done,
}

/// Expand a collected [`GenerationResponse`] into the chunk sequence
/// [`LlmProvider::stream_with_tools`]'s default implementation replays.
pub fn response_chunks(response: GenerationResponse) -> Vec<StreamChunk> {
    let mut chunks = Vec::new();
    match response {
        GenerationResponse::Text { content, .. } => {
            if !content.is_empty() {
                chunks.push(StreamChunk::Text(content));
            }
        }
        GenerationResponse::ToolCalls { calls, .. } => {
            for (index, call) in calls.iter().enumerate() {
                chunks.push(StreamChunk::ToolCallStart {
                    index,
                    id: call.id.clone(),
                    name: call.tool_name.clone(),
                });
                chunks.push(StreamChunk::ToolCallDelta {
                    index,
                    arguments: call.arguments.to_string(),
                });
            }
        }
        GenerationResponse::Mixed { content, calls, .. } => {
            if !content.is_empty() {
                chunks.push(StreamChunk::Text(content));
            }
            for (index, call) in calls.iter().enumerate() {
                chunks.push(StreamChunk::ToolCallStart {
                    index,
                    id: call.id.clone(),
                    name: call.tool_name.clone(),
                });
                chunks.push(StreamChunk::ToolCallDelta {
                    index,
                    arguments: call.arguments.to_string(),
                });
            }
        }
    }
    chunks.push(StreamChunk::Done);
    chunks
}

#[cfg(test)]
mod coverage_response_chunks {
    //! `response_chunks` is the bridge between a collected
    //! `GenerationResponse` and the streaming shape the trait's
    //! default `stream_with_tools` replays. A regression here
    //! changes the order or count of chunks a caller sees from a
    //! provider that has not overridden the default — the engine's
    //! assembly loop then mis-parses the response without any
    //! error pointing at the cause.
    use super::*;
    use kod_types::ToolCall;

    fn call(name: &str, id: Option<&str>) -> ToolCall {
        ToolCall {
            id: id.map(|s| s.to_string()),
            tool_name: name.to_string(),
            arguments: serde_json::json!({"k": "v"}),
        }
    }

    #[test]
    fn text_response_yields_one_text_chunk_then_done() {
        let r = GenerationResponse::Text {
            content: "hello".to_string(),
            usage: None,
        };
        let chunks = response_chunks(r);
        assert_eq!(chunks.len(), 2);
        assert!(matches!(&chunks[0], StreamChunk::Text(t) if t == "hello"));
        assert!(matches!(&chunks[1], StreamChunk::Done));
    }

    #[test]
    fn empty_text_response_yields_only_done() {
        let r = GenerationResponse::Text {
            content: String::new(),
            usage: None,
        };
        let chunks = response_chunks(r);
        assert_eq!(chunks.len(), 1);
        assert!(matches!(&chunks[0], StreamChunk::Done));
    }

    #[test]
    fn tool_calls_response_yields_start_and_delta_per_call_then_done() {
        let r = GenerationResponse::ToolCalls {
            calls: vec![call("a", Some("id_a")), call("b", None)],
            usage: None,
        };
        let chunks = response_chunks(r);
        // 2 calls * 2 chunks + 1 Done = 5.
        assert_eq!(chunks.len(), 5);
        match &chunks[0] {
            StreamChunk::ToolCallStart { index, id, name } => {
                assert_eq!(*index, 0);
                assert_eq!(id.as_deref(), Some("id_a"));
                assert_eq!(name, "a");
            }
            other => panic!("expected ToolCallStart, got {other:?}"),
        }
        match &chunks[1] {
            StreamChunk::ToolCallDelta { index, arguments } => {
                assert_eq!(*index, 0);
                assert!(arguments.contains("k"));
            }
            other => panic!("expected ToolCallDelta, got {other:?}"),
        }
        match &chunks[2] {
            StreamChunk::ToolCallStart { index, id, name } => {
                assert_eq!(*index, 1);
                assert!(id.is_none());
                assert_eq!(name, "b");
            }
            other => panic!("expected ToolCallStart, got {other:?}"),
        }
        assert!(matches!(&chunks[4], StreamChunk::Done));
    }

    #[test]
    fn mixed_response_puts_text_before_tool_chunks() {
        let r = GenerationResponse::Mixed {
            content: "prefix".to_string(),
            calls: vec![call("t", None)],
            usage: None,
        };
        let chunks = response_chunks(r);
        assert_eq!(chunks.len(), 4);
        assert!(matches!(&chunks[0], StreamChunk::Text(t) if t == "prefix"));
        assert!(matches!(&chunks[1], StreamChunk::ToolCallStart { .. }));
        assert!(matches!(&chunks[2], StreamChunk::ToolCallDelta { .. }));
        assert!(matches!(&chunks[3], StreamChunk::Done));
    }

    #[test]
    fn empty_mixed_response_skips_the_text_chunk() {
        let r = GenerationResponse::Mixed {
            content: String::new(),
            calls: vec![call("t", None)],
            usage: None,
        };
        let chunks = response_chunks(r);
        // No text chunk; one ToolCallStart, one delta, one Done.
        assert_eq!(chunks.len(), 3);
        assert!(matches!(&chunks[0], StreamChunk::ToolCallStart { .. }));
    }

    #[test]
    fn done_is_always_last() {
        // The assembly loop relies on this; a regression that put
        // Done before the calls would silently truncate every
        // multi-call response.
        for r in [
            GenerationResponse::Text {
                content: "t".into(),
                usage: None,
            },
            GenerationResponse::ToolCalls {
                calls: vec![call("a", None), call("b", None)],
                usage: None,
            },
            GenerationResponse::Mixed {
                content: "t".into(),
                calls: vec![call("a", None)],
                usage: None,
            },
        ] {
            let chunks = response_chunks(r);
            assert!(
                matches!(chunks.last(), Some(StreamChunk::Done)),
                "Done is not last",
            );
        }
    }

    #[test]
    fn usage_accessor_reports_the_variant_payload() {
        let u = TokenUsage {
            prompt_tokens: 10,
            completion_tokens: 5,
            total_tokens: 15,
            ..Default::default()
        };
        let r = GenerationResponse::Text {
            content: String::new(),
            usage: Some(u.clone()),
        };
        assert_eq!(r.usage(), Some(&u));
        let r = GenerationResponse::ToolCalls {
            calls: vec![],
            usage: Some(u.clone()),
        };
        assert_eq!(r.usage(), Some(&u));
        let r = GenerationResponse::Mixed {
            content: String::new(),
            calls: vec![],
            usage: Some(u.clone()),
        };
        assert_eq!(r.usage(), Some(&u));
    }

    #[test]
    fn usage_accessor_returns_none_when_absent() {
        let r = GenerationResponse::Text {
            content: String::new(),
            usage: None,
        };
        assert!(r.usage().is_none());
    }
}
