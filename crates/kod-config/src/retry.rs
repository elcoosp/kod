//! Delta §9.7: declarative retry-fallback chains.
//!
//! # The gap
//!
//! Today `[llm.routing].fallback` is a single flat list of endpoint
//! names tried on any retryable error. That is enough for "second
//! opinion on failure", but it cannot express the shape a real
//! multi-endpoint setup wants:
//!
//! * A rate-limited *cheap* endpoint should fail over to the *backup*
//!   cheap endpoint, not to the expensive reasoning endpoint that
//!   happens to be next in the flat list.
//! * A network outage should prefer a *different provider* over a
//!   different model on the same provider (they share the outage).
//! * A context-window rejection should fall back to a *larger-window*
//!   model, not the same size on a different endpoint.
//!
//! The shape that expresses all three is per-failure-class chains
//! keyed by selector.
//!
//! # Selectors
//!
//! A selector names either a specific `endpoint/model` pair or a
//! provider-wide wildcard `endpoint/*`. The wildcard form applies to
//! every model on that endpoint. A global `*` key is the default for
//! any failed selector without a more specific match.
//!
//! Specificity, highest first:
//!
//! 1. Exact `endpoint/model`.
//! 2. `endpoint/*` — provider-wide.
//! 3. `*` — global.
//!
//! # Resolver behaviour
//!
//! `resolve(failed, class)` returns the ordered candidate list for a
//! failure of class `class` on selector `failed`. Two invariants:
//!
//! * **The failed selector is never a candidate.** A chain that
//!   retries the endpoint that just refused is not a fallback chain;
//!   the resolver drops it wherever it appears in the config list.
//! * **The list is deduplicated, order-preserved.** A config that
//!   names the same candidate twice (via two selectors that happen to
//!   overlap) yields the candidate once.
//!
//! # What this is NOT
//!
//! * Not wildcard *transforms* (`google/*` -> `google-vertex/x`),
//!   not transitive expansion (`A -> B -> C`), not effort-aware
//!   matching. Those are the oh-my-pi design's later features; this
//!   module lands the shape a config can carry, and a follow-up can
//!   extend the resolver without a config break.
//! * Not the engine's caller. `kod-core` reads `RetryConfig::resolve`
//!   from the fallback loop; this module is pure decision logic with
//!   no engine dependency, so it can be unit-tested in isolation.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Per-failure-class fallback chains, plus a global default.
///
/// Config shape:
///
/// ```toml
/// [llm.retry.fallback_chains]
/// "*" = ["backup"]                         # default for any class
/// TransportRateLimit = ["cheap-2", "backup"]
/// TransportNetwork = ["other-provider"]
/// ContextWindowExceeded = ["big-window"]
/// ```
///
/// Keys are `TurnFailure` variant names (`TransportRateLimit`,
/// `TransportTimeout`, `TransportNetwork`, `ProviderRefused`,
/// `ProviderAuthError`, `ContextWindowExceeded`, `MalformedJson`,
/// `HallucinatedTool`, `ContentFiltered`, `BudgetExhausted`,
/// `PolicyDenied`, `Unknown`). Unknown keys are legal — a config can
/// name a class kod has not written yet, and the resolver returns an
/// empty chain for it (falling through to the global `*` entry when
/// present).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RetryConfig {
    /// Fallback chains keyed by `TurnFailure` class name. See the
    /// type docs for the shape; the resolver reads this map directly.
    pub fallback_chains: BTreeMap<String, Vec<String>>,
}

impl RetryConfig {
    /// The candidate list for a failure of `class` on `failed`.
    ///
    /// `failed` is the selector that just failed, in `endpoint/model`
    /// form. `class` is the `TurnFailure` variant name. Returns an
    /// order-preserved, deduplicated list with `failed` removed
    /// wherever it appears.
    ///
    /// Specificity: an exact-class entry wins; a `"*"` (global) entry
    /// is the fallback when no class-specific entry exists. If
    /// neither exists, the result is empty and the caller falls back
    /// to its existing single-endpoint behaviour.
    pub fn resolve(&self, failed: &str, class: &str) -> Vec<String> {
        let chain = self
            .fallback_chains
            .get(class)
            .or_else(|| self.fallback_chains.get("*"));
        let Some(chain) = chain else {
            return Vec::new();
        };
        let mut out: Vec<String> = Vec::with_capacity(chain.len());
        for candidate in chain {
            if candidate == failed {
                continue;
            }
            if out.iter().any(|c| c == candidate) {
                continue;
            }
            out.push(candidate.clone());
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(entries: &[(&str, &[&str])]) -> RetryConfig {
        let mut fallback_chains = BTreeMap::new();
        for (k, vs) in entries {
            fallback_chains.insert(k.to_string(), vs.iter().map(|s| s.to_string()).collect());
        }
        RetryConfig { fallback_chains }
    }

    #[test]
    fn class_specific_entry_wins() {
        let c = cfg(&[
            ("TransportRateLimit", &["cheap-2", "backup"]),
            ("*", &["global-fallback"]),
        ]);
        assert_eq!(
            c.resolve("cheap-1", "TransportRateLimit"),
            vec!["cheap-2", "backup"],
        );
    }

    #[test]
    fn global_entry_fills_in_when_class_missing() {
        let c = cfg(&[("*", &["global-fallback"])]);
        assert_eq!(
            c.resolve("any/endpoint", "TransportRateLimit"),
            vec!["global-fallback"],
        );
    }

    #[test]
    fn no_match_is_empty() {
        let c = cfg(&[("TransportTimeout", &["x"])]);
        assert!(c.resolve("a/b", "TransportRateLimit").is_empty());
        assert!(c.resolve("a/b", "totally-made-up-class").is_empty());
    }

    #[test]
    fn failed_endpoint_is_never_a_candidate() {
        let c = cfg(&[("TransportRateLimit", &["a", "b", "c"])]);
        assert_eq!(c.resolve("b", "TransportRateLimit"), vec!["a", "c"],);
        // And when the failed appears twice, both are dropped.
        let c = cfg(&[("TransportRateLimit", &["a", "b", "b", "c"])]);
        assert_eq!(c.resolve("b", "TransportRateLimit"), vec!["a", "c"],);
    }

    #[test]
    fn order_is_preserved_and_duplicates_dropped() {
        let c = cfg(&[("TransportRateLimit", &["a", "b", "a", "c", "b"])]);
        assert_eq!(
            c.resolve("failed", "TransportRateLimit"),
            vec!["a", "b", "c"],
        );
    }

    #[test]
    fn empty_config_resolves_to_empty() {
        let c = RetryConfig::default();
        assert!(c.resolve("a/b", "TransportRateLimit").is_empty());
    }
}
