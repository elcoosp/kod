//! Structured completion request (AD-01).
//!
//! The `LlmProvider` trait historically accepted a single `&str` prompt.
//! That shape made two things impossible:
//!
//! - Native prompt caching. Anthropic's `cache_control` and OpenAI's
//!   automatic prefix caching both need structured messages, not one
//!   concatenated string.
//! - Multiple models per session. A single prompt string is fine when
//!   there is one provider and one model; it is not fine when the swarm
//!   routes "planner" to one endpoint and "coder" to another.
//!
//! `CompletionRequest` is the target shape. During the D1 migration, a
//! `LlmProvider` may still serve the legacy `&str` API — see the
//! `complete` default implementation in `traits.rs`, which renders the
//! structured request back to text and delegates. Once every provider
//! implements `complete`, the legacy methods are dropped.
//!
//! Imported by providers and the engine; not by tools. `kod-tools`
//! never sees a completion request.

use kod_types::{ChatMessage, ToolDefinition};
use serde::{Deserialize, Serialize};

use crate::traits::GenerationOptions;

/// A named endpoint + model pair. The `endpoint` field is the config
/// key (`"local-ollama"`, `"anthropic"`), the `model` field is the
/// provider's own name for the model (`"qwen2.5-coder:32b"`,
/// `"claude-sonnet-4-5"`).
///
/// `ModelRef` is deliberately cheap to clone and hash: the router and
/// the fallback chain both key on it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ModelRef {
    pub endpoint: String,
    pub model: String,
}

impl ModelRef {
    pub fn new(endpoint: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            model: model.into(),
        }
    }

    /// `"local-ollama/qwen2.5-coder:32b"` — for logs, tool headers,
    /// and the `/model` display.
    pub fn display(&self) -> String {
        format!("{}/{}", self.endpoint, self.model)
    }
}

/// One cacheable or volatile slice of the system prompt.
///
/// The router builds this from the sections it already produces:
/// identity + repo map are cacheable; environment + tool inventory are
/// volatile. The flag is a *hint to the provider* — a provider that
/// does not support explicit caching simply renders `text` in order.
#[derive(Debug, Clone)]
pub struct SystemSegment {
    pub text: String,
    /// `true` when the segment is part of the invariant prefix across
    /// turns in a session. A provider with explicit cache support
    /// (Anthropic `cache_control`) places a cache breakpoint at the
    /// **last** cacheable segment; providers with implicit prefix
    /// caching (OpenAI) ignore the flag and rely on byte-stable
    /// ordering of the cacheable segments.
    pub cacheable: bool,
}

/// Ordered list of system segments. Rendered in order; the volatile
/// segments always come after the cacheable ones, so the cacheable
/// prefix is byte-stable across turns.
#[derive(Debug, Clone, Default)]
pub struct SystemPrompt {
    pub segments: Vec<SystemSegment>,
}

impl SystemPrompt {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a segment. Returns `self` for chaining.
    pub fn with(mut self, text: impl Into<String>, cacheable: bool) -> Self {
        self.segments.push(SystemSegment {
            text: text.into(),
            cacheable,
        });
        self
    }

    /// Concatenate every segment's text with a blank line between, in
    /// order. Providers without native system-prompt support use this
    /// to build a single system message.
    pub fn render_text(&self) -> String {
        self.segments
            .iter()
            .map(|s| s.text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    pub fn is_empty(&self) -> bool {
        self.segments.is_empty()
    }
}

/// A complete, provider-agnostic completion request.
#[derive(Debug, Clone)]
pub struct CompletionRequest {
    pub system: SystemPrompt,
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<ToolDefinition>,
    pub options: GenerationOptions,
    pub model: ModelRef,
}

impl CompletionRequest {
    /// Construct a minimal request — useful for tests and for the
    /// migration adapter, which renders this back to the old `&str`
    /// shape. Production callers build the full struct directly.
    pub fn new(model: ModelRef) -> Self {
        Self {
            system: SystemPrompt::new(),
            messages: Vec::new(),
            tools: Vec::new(),
            options: GenerationOptions::default(),
            model,
        }
    }

    /// Render the entire request as a single text prompt, matching the
    /// shape the legacy `LlmProvider::generate_with_tools` callers pass
    /// today (system block, then a transcript). The layout is
    /// deliberately stable: the D1 migration's golden-prefix test
    /// compares this output to the pre-migration engine.
    pub fn render_text(&self) -> String {
        let mut out = String::new();
        if !self.system.is_empty() {
            out.push_str("## System\n\n");
            out.push_str(&self.system.render_text());
            out.push_str("\n\n");
        }
        for m in &self.messages {
            out.push_str(&m.render_text());
            out.push('\n');
        }
        out
    }
}

/// What a provider supports, so the router and the engine can adapt
/// without probing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProviderCapabilities {
    pub tools: bool,
    pub vision: bool,
    pub json_mode: bool,
    pub prompt_cache: PromptCacheKind,
    pub embeddings: bool,
    /// `true` when the provider emits live `Text` chunks during a tool
    /// call. Some OpenAI-compatible servers buffer tool-call rounds and
    /// only stream after the call; the engine's "thinking…" indicator
    /// uses this to avoid stalling the UI.
    pub streaming_tools: bool,
    /// `None` when the provider does not publish pricing. The TUI's
    /// cost accounting reads this; absence disables the `$` display
    /// rather than showing a guess.
    pub pricing: Option<ModelPricing>,
}

impl ProviderCapabilities {
    /// Sensible default: everything on, no prompt-cache hints, no
    /// pricing. Providers override with their real matrix.
    pub fn conservative() -> Self {
        Self {
            tools: true,
            vision: false,
            json_mode: false,
            prompt_cache: PromptCacheKind::None,
            embeddings: false,
            streaming_tools: false,
            pricing: None,
        }
    }
}

impl Default for ProviderCapabilities {
    fn default() -> Self {
        Self::conservative()
    }
}

/// How the provider caches the invariant prefix of a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptCacheKind {
    /// No caching, or the provider does not expose a knob. Cache
    /// behavior depends entirely on the server.
    None,
    /// The server caches on a byte-stable prefix with no client hint
    /// (OpenAI-compatible, most local servers).
    Automatic,
    /// The client marks cache breakpoints explicitly (Anthropic
    /// `cache_control: {"type": "ephemeral"}`).
    Explicit,
}

/// USD per million tokens for input and output.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelPricing {
    pub input_per_mtok_usd: f64,
    pub output_per_mtok_usd: f64,
}

impl ModelPricing {
    pub fn new(input_per_mtok_usd: f64, output_per_mtok_usd: f64) -> Self {
        Self {
            input_per_mtok_usd,
            output_per_mtok_usd,
        }
    }

    /// Cost for a call with the given token counts, in USD.
    pub fn cost_usd(&self, prompt_tokens: usize, completion_tokens: usize) -> f64 {
        (prompt_tokens as f64 / 1_000_000.0) * self.input_per_mtok_usd
            + (completion_tokens as f64 / 1_000_000.0) * self.output_per_mtok_usd
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kod_types::{MessageId, MessageRole};
    use time::OffsetDateTime;

    #[test]
    fn model_ref_display_roundtrips() {
        let r = ModelRef::new("local-ollama", "qwen2.5-coder:32b");
        assert_eq!(r.display(), "local-ollama/qwen2.5-coder:32b");
    }

    #[test]
    fn system_prompt_render_in_order() {
        let s = SystemPrompt::new()
            .with("identity", true)
            .with("repo map", true)
            .with("env", false);
        let out = s.render_text();
        assert!(out.starts_with("identity\n\nrepo map\n\nenv"));
    }

    #[test]
    fn completion_request_render_includes_system_then_messages() {
        let mut req = CompletionRequest::new(ModelRef::new("e", "m"));
        req.system = SystemPrompt::new().with("You are kod.", true);
        req.messages = vec![
            ChatMessage::text(
                MessageId::new(),
                MessageRole::User,
                "hi",
                OffsetDateTime::now_utc(),
            ),
            ChatMessage::text(
                MessageId::new(),
                MessageRole::Assistant,
                "hello",
                OffsetDateTime::now_utc(),
            ),
        ];
        let rendered = req.render_text();
        assert!(rendered.contains("## System"));
        assert!(rendered.contains("You are kod."));
        assert!(rendered.contains("User: hi"));
        assert!(rendered.contains("Assistant: hello"));
    }

    #[test]
    fn pricing_cost_math() {
        let p = ModelPricing::new(3.0, 15.0);
        // 1M input, 1M output = 18 USD
        assert!((p.cost_usd(1_000_000, 1_000_000) - 18.0).abs() < 1e-9);
        // 1000 tokens in, 500 out
        let c = p.cost_usd(1000, 500);
        assert!((c - 0.0105).abs() < 1e-9);
    }
}
