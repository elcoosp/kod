//! Model catalog metadata (borrow from oh-my-pi, delta §13.1).
//!
//! # Why a catalog
//!
//! A provider's `list_models` returns wire identifiers. The engine
//! wants more: how big the context window is, what a cache write
//! costs under each TTL, whether the model supports a reasoning
//! effort ladder, and (for swarm role routing) a rough capability
//! score. Some of that is discoverable from the provider; most of it
//! is not, and providers disagree about the shape.
//!
//! The catalog is a small, hand-maintained table of the well-known
//! models. It is *fallback*, not truth: a caller that has a value
//! from the provider uses that; the catalog answers only when the
//! provider is silent. That keeps the table small (a dozen models,
//! not thousands) while giving the routing and cost layers a sane
//! default for the models users actually run.
//!
//! # Dialect-tolerant resolution
//!
//! A model id travels in many spellings: `claude-3-5-sonnet-latest`,
//! `claude-3.5-sonnet`, `anthropic/claude-3-5-sonnet-latest`,
//! `openrouter/anthropic/claude-3-5-sonnet`. [`resolve`] tries, in
//! order: exact id match, then normalised (dashes/underscores/dots
//! folded, `-latest` stripped, provider prefixes stripped), then a
//! longest-prefix match against the known ids. A miss returns
//! `None` — the caller falls back to provider-reported values, and
//! from there to config defaults.
//!
//! # What this is NOT
//!
//! * Not a price book. The numbers are the providers' published
//!   rates as of the crate's last edit; a user with a custom rate
//!   sets `[endpoint.pricing]` and the config wins.
//! * Not a wire layer. The types mirror `ModelInfo` /
//!   `ModelPricing`, but the catalog converts to those; nothing here
//!   touches a request.

use crate::request::ModelPricing;
use crate::traits::LongContextPricing;
use kod_types::effort::EffortLevel;

/// Peak/off-peak pricing (delta §13.1). UTC hours, [start, end)
/// inclusive of start and exclusive of end. An empty `peak_windows`
/// means "no time-based pricing".
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TimeBasedPricing {
    /// One or more `(start_hour, end_hour)` windows in UTC, 0..=24.
    /// `(0, 8)` is the 00:00–08:00 UTC window.
    pub peak_windows: Vec<(u8, u8)>,
    /// Multiplier applied to every rate *outside* the peak windows.
    /// A value of 0.5 means an off-peak call bills at half price.
    pub off_peak_multiplier: f64,
}

impl TimeBasedPricing {
    /// Whether `utc_hour` falls inside a peak window.
    pub fn is_peak(&self, utc_hour: u8) -> bool {
        self.peak_windows
            .iter()
            .any(|(s, e)| utc_hour >= *s && utc_hour < *e)
    }

    /// Multiplier for `utc_hour`: 1.0 inside a peak window, the
    /// off-peak multiplier outside.
    pub fn multiplier_for(&self, utc_hour: u8) -> f64 {
        if self.is_peak(utc_hour) {
            1.0
        } else {
            self.off_peak_multiplier
        }
    }
}

/// One catalog entry.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ModelMeta {
    /// The canonical wire identifier (no provider prefix).
    pub id: String,
    /// Context window in tokens.
    pub context_window: usize,
    /// Max output tokens, when the provider documents one.
    #[serde(default)]
    pub max_output: Option<usize>,
    /// The per-million-token rates with the cache tiers filled in.
    pub pricing: ModelPricing,
    /// The reasoning-effort ladder, ascending. `None` for a model
    /// with no reasoning control.
    #[serde(default)]
    pub efforts: Option<Vec<EffortLevel>>,
    /// A rough capability score, for swarm role routing.
    #[serde(default)]
    pub intelligence: Option<f64>,
    /// Tokens per second, for latency-aware selection.
    #[serde(default)]
    pub tps: Option<f64>,
    /// The over-threshold pricing tier, when the model charges one.
    #[serde(default)]
    pub long_context: Option<LongContextPricing>,
    /// Time-of-day pricing, when the provider has it.
    #[serde(default)]
    pub time_based: Option<TimeBasedPricing>,
}

/// The provider-priority rank (delta §13.1). A caller that has two
/// endpoints serving the same model id prefers the higher rank:
/// first-party beats an aggregator beats a generic gateway, because
/// the rate, the cache behaviour, and the wire shape are all more
/// predictable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ProviderPriority {
    /// A generic gateway or a self-hosted proxy.
    Gateway = 0,
    /// A multi-provider aggregator (OpenRouter and the like).
    Aggregator = 1,
    /// The vendor that trained the model.
    FirstParty = 2,
}

/// Rank an endpoint by provider kind. The mapping is by the
/// `provider` field a `kod_config::EndpointConfig` names; a caller
/// that has a `ProviderKind` converts it to a string and calls this.
pub fn provider_priority(provider_kind: &str) -> ProviderPriority {
    match provider_kind.to_ascii_lowercase().as_str() {
        "anthropic" | "openai" | "google" | "gemini" => ProviderPriority::FirstParty,
        "openrouter" | "together" | "groq" | "fireworks" => ProviderPriority::Aggregator,
        _ => ProviderPriority::Gateway,
    }
}

/// Normalize a model id for comparison: lowercase, strip a trailing
/// `-latest`, drop any provider-prefix path segments, and fold
/// dashes / underscores / dots into a single `-`.
fn normalize(id: &str) -> String {
    let s = id.trim().to_ascii_lowercase();
    // Strip leading path components (`anthropic/claude-3-5-sonnet`
    // becomes `claude-3-5-sonnet`).
    let s = match s.rsplit_once('/') {
        Some((_, tail)) => tail.to_string(),
        None => s,
    };
    // Strip a trailing `-latest`.
    let s = s.strip_suffix("-latest").unwrap_or(&s).to_string();
    // Fold separators.
    let folded: String = s
        .chars()
        .map(|c| if c == '_' || c == '.' { '-' } else { c })
        .collect();
    // Collapse repeated dashes.
    let mut out = String::with_capacity(folded.len());
    let mut last_dash = false;
    for c in folded.chars() {
        if c == '-' {
            if !last_dash {
                out.push('-');
                last_dash = true;
            }
        } else {
            out.push(c);
            last_dash = false;
        }
    }
    out
}

/// The built-in catalog. Small on purpose: a dozen well-known models
/// whose metadata the routing and cost layers actually consult.
///
/// Numbers are the providers' published USD-per-million-token rates
/// as of the crate's last edit; a user with a custom rate overrides
/// via config. `intelligence` is a rough score (0–100) used only for
/// ordering, not for anything a user sees as a number.
pub fn builtin() -> &'static [ModelMeta] {
    // A `OnceLock` so the vector is built once; the values are
    // constants, so a plain static with lazy init is enough.
    static CATALOG: std::sync::OnceLock<Vec<ModelMeta>> = std::sync::OnceLock::new();
    CATALOG.get_or_init(|| {
        let mut v = Vec::new();
        // Anthropic Claude 4 family.
        v.push(ModelMeta {
            id: "claude-opus-4".into(),
            context_window: 200_000,
            max_output: Some(32_000),
            pricing: ModelPricing::new(15.0, 75.0),
            efforts: Some(vec![
                EffortLevel::Low,
                EffortLevel::Medium,
                EffortLevel::High,
            ]),
            intelligence: Some(95.0),
            tps: Some(30.0),
            long_context: None,
            time_based: None,
        });
        v.push(ModelMeta {
            id: "claude-sonnet-4".into(),
            context_window: 200_000,
            max_output: Some(64_000),
            pricing: ModelPricing::new(3.0, 15.0),
            efforts: Some(vec![
                EffortLevel::Low,
                EffortLevel::Medium,
                EffortLevel::High,
            ]),
            intelligence: Some(85.0),
            tps: Some(55.0),
            long_context: None,
            time_based: None,
        });
        v.push(ModelMeta {
            id: "claude-haiku-4".into(),
            context_window: 200_000,
            max_output: Some(8_000),
            pricing: ModelPricing::new(0.8, 4.0),
            efforts: None,
            intelligence: Some(70.0),
            tps: Some(90.0),
            long_context: None,
            time_based: None,
        });
        // Anthropic Claude 3.5 family.
        v.push(ModelMeta {
            id: "claude-3-5-sonnet".into(),
            context_window: 200_000,
            max_output: Some(8_192),
            pricing: ModelPricing::new(3.0, 15.0),
            efforts: None,
            intelligence: Some(78.0),
            tps: Some(60.0),
            long_context: None,
            time_based: None,
        });
        v.push(ModelMeta {
            id: "claude-3-5-haiku".into(),
            context_window: 200_000,
            max_output: Some(8_192),
            pricing: ModelPricing::new(0.8, 4.0),
            efforts: None,
            intelligence: Some(65.0),
            tps: Some(100.0),
            long_context: None,
            time_based: None,
        });
        // OpenAI GPT-4o family.
        v.push(ModelMeta {
            id: "gpt-4o".into(),
            context_window: 128_000,
            max_output: Some(16_384),
            pricing: ModelPricing::new(2.5, 10.0),
            efforts: None,
            intelligence: Some(80.0),
            tps: Some(80.0),
            long_context: None,
            time_based: None,
        });
        v.push(ModelMeta {
            id: "gpt-4o-mini".into(),
            context_window: 128_000,
            max_output: Some(16_384),
            pricing: ModelPricing::new(0.15, 0.6),
            efforts: None,
            intelligence: Some(60.0),
            tps: Some(120.0),
            long_context: None,
            time_based: None,
        });
        v.push(ModelMeta {
            id: "gpt-4-turbo".into(),
            context_window: 128_000,
            max_output: Some(4_096),
            pricing: ModelPricing::new(10.0, 30.0),
            efforts: None,
            intelligence: Some(75.0),
            tps: Some(45.0),
            long_context: None,
            time_based: None,
        });
        v.push(ModelMeta {
            id: "o1".into(),
            context_window: 200_000,
            max_output: Some(100_000),
            pricing: ModelPricing::new(15.0, 60.0),
            efforts: Some(vec![
                EffortLevel::Low,
                EffortLevel::Medium,
                EffortLevel::High,
            ]),
            intelligence: Some(93.0),
            tps: Some(20.0),
            long_context: None,
            time_based: None,
        });
        v.push(ModelMeta {
            id: "o1-mini".into(),
            context_window: 128_000,
            max_output: Some(65_536),
            pricing: ModelPricing::new(3.0, 12.0),
            efforts: Some(vec![
                EffortLevel::Low,
                EffortLevel::Medium,
                EffortLevel::High,
            ]),
            intelligence: Some(80.0),
            tps: Some(60.0),
            long_context: None,
            time_based: None,
        });
        // Google Gemini.
        v.push(ModelMeta {
            id: "gemini-1.5-pro".into(),
            context_window: 2_097_152,
            max_output: Some(8_192),
            pricing: ModelPricing::new(1.25, 5.0),
            efforts: None,
            intelligence: Some(80.0),
            tps: Some(60.0),
            long_context: Some(LongContextPricing {
                input_threshold: 128_000,
                input_per_mtok_usd: 2.5,
                output_per_mtok_usd: 10.0,
            }),
            time_based: None,
        });
        v.push(ModelMeta {
            id: "gemini-2.0-flash".into(),
            context_window: 1_048_576,
            max_output: Some(8_192),
            pricing: ModelPricing::new(0.1, 0.4),
            efforts: None,
            intelligence: Some(65.0),
            tps: Some(150.0),
            long_context: None,
            time_based: None,
        });
        v
    })
}

/// Resolve `id` to a catalog entry.
///
/// Order: exact match, then normalized match, then longest-prefix
/// match against a normalized known id. The last rule is what makes
/// `claude-3-5-sonnet-20240620` resolve to `claude-3-5-sonnet`.
pub fn resolve(id: &str) -> Option<&'static ModelMeta> {
    let catalog = builtin();
    // Exact.
    if let Some(m) = catalog.iter().find(|m| m.id == id) {
        return Some(m);
    }
    let want = normalize(id);
    if want.is_empty() {
        return None;
    }
    // Normalized exact.
    if let Some(m) = catalog.iter().find(|m| normalize(&m.id) == want) {
        return Some(m);
    }
    // Longest prefix: pick the catalog entry whose normalized id is
    // the longest prefix of `want` at a dash boundary.
    let mut best: Option<(&ModelMeta, usize)> = None;
    for m in catalog {
        let n = normalize(&m.id);
        if want.starts_with(&n)
            && (want.len() == n.len() || want.as_bytes()[n.len()] == b'-')
        {
            let len = n.len();
            if best.map(|(_, l)| len > l).unwrap_or(true) {
                best = Some((m, len));
            }
        }
    }
    best.map(|(m, _)| m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_exact_id_resolves() {
        let m = resolve("claude-sonnet-4").expect("known");
        assert_eq!(m.id, "claude-sonnet-4");
        assert_eq!(m.context_window, 200_000);
    }

    #[test]
    fn a_provider_prefix_is_stripped() {
        let m = resolve("anthropic/claude-sonnet-4").expect("known");
        assert_eq!(m.id, "claude-sonnet-4");
    }

    #[test]
    fn a_double_prefix_is_stripped() {
        let m = resolve("openrouter/anthropic/claude-sonnet-4").expect("known");
        assert_eq!(m.id, "claude-sonnet-4");
    }

    #[test]
    fn a_dash_underscore_dot_fold_is_tolerated() {
        assert!(resolve("claude_3_5_sonnet").is_some());
        assert!(resolve("claude.3.5.sonnet").is_some());
    }

    #[test]
    fn a_latest_suffix_is_stripped() {
        assert!(resolve("claude-3-5-sonnet-latest").is_some());
    }

    #[test]
    fn a_date_suffix_resolves_by_longest_prefix() {
        let m = resolve("claude-3-5-sonnet-20240620").expect("longest prefix");
        assert_eq!(m.id, "claude-3-5-sonnet");
    }

    #[test]
    fn an_unknown_id_returns_none() {
        assert!(resolve("not-a-real-model").is_none());
        assert!(resolve("").is_none());
    }

    #[test]
    fn the_two_anthropic_models_have_a_reasoning_ladder() {
        for id in ["claude-opus-4", "claude-sonnet-4"] {
            let m = resolve(id).expect("known");
            assert!(m.efforts.is_some(), "{id}");
        }
    }

    #[test]
    fn the_gemini_pro_has_a_long_context_tier() {
        let m = resolve("gemini-1.5-pro").expect("known");
        let lc = m.long_context.as_ref().expect("tier");
        assert!(lc.input_threshold > 0);
        assert!(lc.input_per_mtok_usd > m.pricing.input_per_mtok_usd);
    }

    #[test]
    fn provider_priority_ranks_first_party_above_aggregator_above_gateway() {
        assert!(provider_priority("anthropic") > provider_priority("openrouter"));
        assert!(provider_priority("openrouter") > provider_priority("some-gateway"));
    }

    #[test]
    fn priority_is_case_insensitive() {
        assert_eq!(
            provider_priority("Anthropic"),
            provider_priority("anthropic"),
        );
    }

    #[test]
    fn a_peak_window_recognizes_its_hours() {
        let t = TimeBasedPricing {
            peak_windows: vec![(0, 8), (16, 24)],
            off_peak_multiplier: 0.5,
        };
        assert!(t.is_peak(0));
        assert!(t.is_peak(7));
        assert!(!t.is_peak(8));
        assert!(!t.is_peak(15));
        assert!(t.is_peak(16));
        assert!(t.is_peak(23));
    }

    #[test]
    fn the_multiplier_is_one_in_peak_and_the_off_peak_value_otherwise() {
        let t = TimeBasedPricing {
            peak_windows: vec![(0, 8)],
            off_peak_multiplier: 0.5,
        };
        assert_eq!(t.multiplier_for(4), 1.0);
        assert_eq!(t.multiplier_for(12), 0.5);
    }

    #[test]
    fn a_model_with_no_time_based_pricing_has_none() {
        let m = resolve("claude-sonnet-4").expect("known");
        assert!(m.time_based.is_none());
    }

    #[test]
    fn the_catalog_is_not_empty() {
        assert!(!builtin().is_empty());
    }

    #[test]
    fn every_catalog_id_resolves_to_itself() {
        for m in builtin() {
            let back = resolve(&m.id).expect("self-resolution");
            assert_eq!(back.id, m.id);
        }
    }

    #[test]
    fn normalize_collapses_separators() {
        assert_eq!(normalize("Claude__3--5.Sonnet"), "claude-3-5-sonnet");
    }

    #[test]
    fn normalize_strips_a_path_prefix() {
        assert_eq!(normalize("a/b/c"), "c");
    }
}
