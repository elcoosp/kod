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
    /// P0 cache control: when `true` (the default), a provider with
    /// explicit caching (Anthropic) places a cache breakpoint at the
    /// end of the transcript in addition to the one at the last
    /// cacheable system segment. The engine clears this for the one
    /// round after a prefix-changing event (a tool-filter change, a
    /// steered turn), so the provider does not pay a cache-write
    /// premium for a prefix that is about to become stale anyway.
    ///
    /// Providers without explicit caching ignore it.
    pub cache_transcript: bool,
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
            cache_transcript: true,
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
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ModelPricing {
    pub input_per_mtok_usd: f64,
    pub output_per_mtok_usd: f64,
    /// USD per million tokens read from the provider's KV cache.
    ///
    /// `#[serde(default)]` plus a constructor default of
    /// `input_per_mtok_usd * DEFAULT_CACHE_READ_RATIO` means a config
    /// written before this field existed loads with the provider's
    /// published ratio (Anthropic: 0.1x). A caller that sets the
    /// field explicitly keeps its value.
    #[serde(default = "default_cache_read_rate")]
    pub cache_read_per_mtok_usd: f64,
    /// USD per million tokens written to the provider's KV cache.
    /// Default ratio is 1.25x input (Anthropic's cache-write premium).
    #[serde(default = "default_cache_write_rate")]
    pub cache_write_per_mtok_usd: f64,
    /// How the provider reports cache tokens. Defaults to `Split`
    /// (Anthropic); OpenAI-compatible endpoints override to `Subset`
    /// at provider construction. A config written before this field
    /// existed loads as `Split`, which is the pre-change behavior.
    #[serde(default)]
    pub cache_convention: crate::CacheConvention,
}

/// Default cache-read rate as a fraction of the full input rate.
/// Anthropic's published ratio; OpenAI's automatic caching is 0.5x,
/// but a caller with a `[pricing]` block on an OpenAI endpoint can
/// set the field explicitly.
const DEFAULT_CACHE_READ_RATIO: f64 = 0.1;
/// Default cache-write rate as a fraction of the full input rate.
/// Anthropic charges 1.25x input to write a cache entry.
const DEFAULT_CACHE_WRITE_RATIO: f64 = 1.25;

// Serde default helpers cannot read sibling fields, so they return
// the ratios applied to a *nominal* $1.00/M input rate. This is
// correct only when the caller set `input_per_mtok_usd` explicitly
// and did not override the cache rates; `ModelPricing::new` below
// is the authoritative path for the common case, and every kod
// config goes through it.
fn default_cache_read_rate() -> f64 {
    DEFAULT_CACHE_READ_RATIO
}
fn default_cache_write_rate() -> f64 {
    DEFAULT_CACHE_WRITE_RATIO
}

impl ModelPricing {
    /// Build pricing from the two rates a config file has always
    /// carried, deriving the cache tiers from the published ratios.
    pub fn new(input_per_mtok_usd: f64, output_per_mtok_usd: f64) -> Self {
        Self {
            input_per_mtok_usd,
            output_per_mtok_usd,
            cache_read_per_mtok_usd: input_per_mtok_usd * DEFAULT_CACHE_READ_RATIO,
            cache_write_per_mtok_usd: input_per_mtok_usd * DEFAULT_CACHE_WRITE_RATIO,
            cache_convention: crate::CacheConvention::Split,
        }
    }

    /// Build pricing with explicit cache rates, for a caller whose
    /// endpoint charges a non-default ratio.
    pub fn with_cache_rates(
        input_per_mtok_usd: f64,
        output_per_mtok_usd: f64,
        cache_read_per_mtok_usd: f64,
        cache_write_per_mtok_usd: f64,
    ) -> Self {
        Self {
            input_per_mtok_usd,
            output_per_mtok_usd,
            cache_read_per_mtok_usd,
            cache_write_per_mtok_usd,
            cache_convention: crate::CacheConvention::Split,
        }
    }

    /// Set the cache convention for this pricing. Consuming builder —
    /// chain after `new` or `with_cache_rates`. `Split` is the
    /// default; OpenAI-compatible endpoints override to `Subset`
    /// because their `prompt_tokens` includes cached tokens.
    pub fn with_cache_convention(mut self, conv: crate::CacheConvention) -> Self {
        self.cache_convention = conv;
        self
    }

    /// Cost for a call with the given token counts, in USD.
    ///
    /// Kept for callers that only have the two-token-count shape;
    /// delegates to [`Self::cost_for_usage`] with zero cache tokens
    /// so both paths agree on the arithmetic.
    pub fn cost_usd(&self, prompt_tokens: usize, completion_tokens: usize) -> f64 {
        self.cost_for_usage(&crate::TokenUsage {
            prompt_tokens,
            completion_tokens,
            total_tokens: prompt_tokens.saturating_add(completion_tokens),
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
        })
    }

    /// Cost for a full usage report, separating full-price input,
    /// cache reads, cache writes, and output.
    ///
    /// `uncached_input_tokens` is derived, not passed: a caller that
    /// has a `TokenUsage` should never have to recompute the split,
    /// and every kod call site has one.
    pub fn cost_for_usage(&self, usage: &crate::TokenUsage) -> f64 {
        let m = 1_000_000.0;
        // The convention decides what `prompt_tokens` already contains.
        // Split: prompt is the *uncached* portion already (Anthropic's
        // `input_tokens`). Subset: prompt includes cache_read, so the
        // fresh portion is prompt − cache_read.
        let fresh = match self.cache_convention {
            crate::CacheConvention::Split => usage.uncached_input_tokens(),
            crate::CacheConvention::Subset => {
                usage.prompt_tokens.saturating_sub(usage.cache_read_tokens)
            }
        };
        (fresh as f64 / m) * self.input_per_mtok_usd
            + (usage.cache_read_tokens as f64 / m) * self.cache_read_per_mtok_usd
            + (usage.cache_creation_tokens as f64 / m) * self.cache_write_per_mtok_usd
            + (usage.completion_tokens as f64 / m) * self.output_per_mtok_usd
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

#[cfg(test)]
mod coverage_prompt_types {
    //! `SystemPrompt`, `ModelPricing`, and `ProviderCapabilities` are
    //! the D1 migration's public contract. The engine builds one, the
    //! TUI reads the other two. A regression in any of them is
    //! invisible until a specific endpoint's accounting drifts, so
    //! each behaviour is pinned.
    use super::*;

    #[test]
    fn system_prompt_with_chains_in_order() {
        let s = SystemPrompt::new()
            .with("first", true)
            .with("second", false)
            .with("third", true);
        assert_eq!(s.segments.len(), 3);
        assert_eq!(s.segments[0].text, "first");
        assert!(s.segments[0].cacheable);
        assert!(!s.segments[1].cacheable);
        assert!(s.segments[2].cacheable);
    }

    #[test]
    fn system_prompt_empty_renders_to_empty_string() {
        assert!(SystemPrompt::new().is_empty());
        assert_eq!(SystemPrompt::new().render_text(), "");
    }

    #[test]
    fn system_prompt_render_separates_segments_with_blank_line() {
        let s = SystemPrompt::new().with("a", true).with("b", false);
        assert_eq!(s.render_text(), "a\n\nb");
    }

    #[test]
    fn model_ref_display_is_endpoint_slash_model() {
        assert_eq!(
            ModelRef::new("local-ollama", "qwen2.5:7b").display(),
            "local-ollama/qwen2.5:7b",
        );
    }

    #[test]
    fn model_ref_equality_and_hash_use_both_fields() {
        use std::collections::HashSet;
        let a = ModelRef::new("e", "m");
        let b = ModelRef::new("e", "m");
        let c = ModelRef::new("e", "other");
        assert_eq!(a, b);
        assert_ne!(a, c);
        let mut set = HashSet::new();
        set.insert(a);
        assert!(set.contains(&b));
        assert!(!set.contains(&c));
    }

    #[test]
    fn pricing_zero_tokens_is_zero_cost() {
        let p = ModelPricing::new(3.0, 15.0);
        assert_eq!(p.cost_usd(0, 0), 0.0);
    }

    #[test]
    fn pricing_splits_input_and_output() {
        let p = ModelPricing::new(1.0, 10.0);
        assert!((p.cost_usd(1_000_000, 0) - 1.0).abs() < 1e-9);
        assert!((p.cost_usd(0, 1_000_000) - 10.0).abs() < 1e-9);
        assert!((p.cost_usd(1_000_000, 1_000_000) - 11.0).abs() < 1e-9);
    }

    #[test]
    fn pricing_cost_is_additive_across_calls() {
        let p = ModelPricing::new(3.0, 15.0);
        let a = p.cost_usd(500_000, 250_000);
        let b = p.cost_usd(500_000, 250_000);
        let c = p.cost_usd(1_000_000, 500_000);
        assert!((a + b - c).abs() < 1e-9);
    }

    #[test]
    fn conservative_capabilities_enable_tools_and_disable_pricing() {
        let caps = ProviderCapabilities::conservative();
        assert!(caps.tools);
        assert!(!caps.streaming_tools);
        assert!(caps.pricing.is_none());
        assert_eq!(caps.prompt_cache, PromptCacheKind::None);
    }

    #[test]
    fn capabilities_default_matches_conservative() {
        // The two are used interchangeably by call sites that want
        // "sensible defaults"; if they ever diverge, half the
        // workspace gets one shape and half the other.
        assert_eq!(
            ProviderCapabilities::default(),
            ProviderCapabilities::conservative(),
        );
    }
}

#[cfg(test)]
mod cache_convention_tests {
    use super::*;


    #[test]
    fn split_convention_bills_fresh_at_full_and_cache_at_discount() {
        // kod's `prompt_tokens` is the *total* input window — Anthropic's
        // `input + cache_read + cache_creation` folded into one number at
        // the wire layer. The Split convention recovers the fresh portion
        // by subtracting both cache fields, so a 6M window with 5M served
        // from cache bills 1M fresh + 5M read.
        let pricing = ModelPricing {
            input_per_mtok_usd: 3.0,
            output_per_mtok_usd: 15.0,
            cache_read_per_mtok_usd: 0.3,
            cache_write_per_mtok_usd: 3.75,
            cache_convention: crate::CacheConvention::Split,
        };
        let usage = crate::TokenUsage {
            prompt_tokens: 6_000_000,      // total window = fresh + read
            completion_tokens: 0,
            total_tokens: 6_000_000,
            cache_read_tokens: 5_000_000,
            cache_creation_tokens: 0,
        };
        // fresh = 6M - 5M - 0 = 1M → 1M * 3 + 5M * 0.3 = 3 + 1.5 = 4.5
        assert!((pricing.cost_for_usage(&usage) - 4.5).abs() < 1e-9);
    }

    #[test]
    fn subset_convention_bills_cached_portion_at_the_discount() {
        // OpenAI shape: prompt includes cached tokens.
        let pricing = ModelPricing {
            input_per_mtok_usd: 3.0,
            output_per_mtok_usd: 15.0,
            cache_read_per_mtok_usd: 0.3,
            cache_write_per_mtok_usd: 3.75,
            cache_convention: crate::CacheConvention::Subset,
        };
        let usage = crate::TokenUsage {
            prompt_tokens: 6_000_000,      // includes the cached 5M
            completion_tokens: 0,
            total_tokens: 6_000_000,
            cache_read_tokens: 5_000_000,
            cache_creation_tokens: 0,
        };
        // fresh = 6M - 5M = 1M * 3 = 3; read 5M * 0.3 = 1.5; total 4.5
        assert!((pricing.cost_for_usage(&usage) - 4.5).abs() < 1e-9);
    }

    #[test]
    fn conventions_diverge_when_cache_creation_is_reported() {
        // Split subtracts BOTH cache fields (read and creation) from the
        // window; Subset subtracts only the read field. Given a usage
        // that reports creation > 0, the two conventions produce
        // different fresh portions and therefore different bills — this
        // is the exact divergence a wrong config would introduce.
        let base = ModelPricing {
            input_per_mtok_usd: 10.0,
            output_per_mtok_usd: 0.0,
            cache_read_per_mtok_usd: 1.0,
            cache_write_per_mtok_usd: 12.5,
            cache_convention: crate::CacheConvention::Split,
        };
        let mut subset = base;
        subset.cache_convention = crate::CacheConvention::Subset;
        let usage = crate::TokenUsage {
            prompt_tokens: 1_000_000,       // total window
            completion_tokens: 0,
            total_tokens: 1_000_000,
            cache_read_tokens: 0,
            cache_creation_tokens: 100_000,
        };
        let split_cost = base.cost_for_usage(&usage);
        let subset_cost = subset.cost_for_usage(&usage);
        // Split: fresh = 1M - 0 - 100k = 900k → 0.9M * 10 = 9.0
        //        + 100k * 12.5 / 1M = 1.25 → total 10.25
        // Subset: fresh = 1M - 0 = 1M → 1M * 10 = 10.0
        //        + 100k * 12.5 / 1M = 1.25 → total 11.25
        assert!((split_cost - 10.25).abs() < 1e-6, "split = {split_cost}");
        assert!((subset_cost - 11.25).abs() < 1e-6, "subset = {subset_cost}");
    }
}
