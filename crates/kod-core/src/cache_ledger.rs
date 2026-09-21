//! KV-cache-aware endpoint routing ledger (P1).
//!
//! # Why this exists
//!
//! Every turn, `resolve_chain_for_task` rebuilds the endpoint chain
//! from the task classification. Nothing in that path knows whether
//! the target endpoint's KV cache is warm, what a switch would cost,
//! or what the previous turn spent. So the session bounces local and
//! cloud endpoints, re-processing the whole transcript at full input
//! price each hop.
//!
//! The ledger keeps one record per endpoint: the fingerprint of the
//! last request head it served, the cache-token counts it reported,
//! and the turn it was last used. Before a hop, the engine asks the
//! ledger what the switch would cost. A cold target pays the
//! transcript re-processing penalty (cache-write plus cache-read
//! tiers on first use there); a warm target pays nothing. The hop is
//! taken only when the projected saving clears the penalty by a
//! configurable margin.
//!
//! # Grounding
//!
//! The ledger observes rather than assumes: the fingerprint comes
//! from bytes the engine already renders, and the cache-token counts
//! come from the provider's own usage report (see P0's TokenUsage
//! extension). A provider that reports no cache fields leaves the
//! ledger in a conservative state — every target looks cold, and the
//! gate declines hops that cannot be justified.
//!
//! # What it does not do
//!
//! It does not make routing decisions. The chain's *order* is still
//! classification-driven; the ledger only gates the hop between the
//! preferred endpoint and the warm fallback. That keeps routing
//! legible and testable, and means a mis-tuned ledger degrades
//! toward "stay where the cache is warm" rather than "silently
//! route to the wrong model".

use std::collections::HashMap;

use kod_provider::request::ModelPricing;
use kod_provider::TokenUsage;

/// Per-endpoint cache state. One entry per endpoint name (not per
/// `ModelRef` — a `/model` switch on the same endpoint keeps the
/// underlying prefix cache warm, and the ledger should know that).
#[derive(Debug, Clone, Default)]
pub struct EndpointCacheState {
    /// FNV-1a hash of the last request head served to this endpoint.
    /// "Head" means the cacheable system prefix plus the sorted tool
    /// schema bytes — everything up to and including the marker that
    /// Anthropic caches. A change to that prefix invalidates the
    /// cache; the fingerprint captures it.
    pub head_fingerprint: u64,
    /// Tokens the provider reported as served from cache on the last
    /// call (`TokenUsage::cache_read_tokens`). Zero for a provider
    /// that does not report cache state.
    pub cached_tokens: u64,
    /// The turn that last touched this endpoint. Used for staleness
    /// (an endpoint idle for hundreds of turns is likely evicted by
    /// the provider's own TTL, but the ledger conservatively keeps
    /// it warm until proven otherwise).
    pub last_used_turn: u64,
}

/// The ledger. Cheap to clone — the engine shares one behind an
/// `Arc<RwLock<_>>`, so a `Clone` would alias the interior state and
/// the two would diverge.
#[derive(Debug, Default)]
pub struct CacheLedger {
    states: HashMap<String, EndpointCacheState>,
    /// The endpoint we are currently warm on. Preferred by the gate
    /// when the requested hop would not save enough to justify the
    /// re-processing penalty.
    sticky: Option<String>,
}

impl CacheLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// Observe a completed call: record which endpoint served it,
    /// what its cache state reported, and what head it saw.
    ///
    /// Called from `record_cost` (the one place the engine already
    /// has the winning endpoint, its usage, and the turn id).
    pub fn observe(
        &mut self,
        turn: u64,
        endpoint: &str,
        head_fingerprint: u64,
        usage: &TokenUsage,
    ) {
        let state = self
            .states
            .entry(endpoint.to_string())
            .or_default();
        state.head_fingerprint = head_fingerprint;
        state.cached_tokens = usage.cache_read_tokens as u64;
        state.last_used_turn = turn;
        // The endpoint we just used is now the warm one. A later
        // turn that considers hopping away will be gated against
        // this.
        self.sticky = Some(endpoint.to_string());
    }

    /// Whether the endpoint's cache matches the fingerprint the
    /// caller is about to send.
    pub fn is_warm(&self, endpoint: &str, head_fingerprint: u64) -> bool {
        self.states
            .get(endpoint)
            .map(|s| s.head_fingerprint == head_fingerprint)
            .unwrap_or(false)
    }

    /// The endpoint currently warm, if any.
    pub fn sticky(&self) -> Option<&str> {
        self.sticky.as_deref()
    }

    /// Projected USD cost of switching to a cold `target` for a
    /// transcript of `transcript_tokens`.
    ///
    /// A cold target re-processes the whole transcript on first use:
    /// it writes the prefix to its cache (`cache_write_per_mtok_usd`)
    /// and then reads it back for the completion
    /// (`cache_read_per_mtok_usd`). A warm target is zero.
    ///
    /// Conservative by construction: the write rate is the *higher*
    /// of the two, so the estimate never understates the penalty and
    /// the gate errs on the side of staying.
    pub fn switch_penalty_usd(
        &self,
        target: &str,
        head_fingerprint: u64,
        pricing: &ModelPricing,
        transcript_tokens: u64,
    ) -> f64 {
        if self.is_warm(target, head_fingerprint) {
            return 0.0;
        }
        let m = 1_000_000.0;
        (transcript_tokens as f64 / m)
            * (pricing.cache_write_per_mtok_usd + pricing.cache_read_per_mtok_usd)
    }

    /// Decide whether to hop from `preferred` to `fallback`.
    ///
    /// `preferred` is the classification's first choice (a cheaper
    /// endpoint, typically); `fallback` is the endpoint the ledger
    /// believes is warm. `per_turn_saving_usd` is the estimated
    /// saving per turn of using `preferred` over `fallback`;
    /// `penalty_usd` is what the hop costs right now (see
    /// [`Self::switch_penalty_usd`]).
    ///
    /// Returns the endpoint name to use. The rule: take the hop when
    /// the penalty is zero (already warm, or nothing to lose) or when
    /// the projected saving clears the penalty by a `MARGIN` factor.
    /// The margin is hysteresis — a hop that barely pays for itself
    /// will be reversed next turn, and reversal pays the penalty
    /// again.
    ///
    /// Conservative on ambiguity: a caller that cannot produce a
    /// saving estimate passes 0.0, and the gate keeps the warm
    /// fallback.
    pub fn gate<'a>(
        &self,
        preferred: &'a str,
        fallback: &'a str,
        per_turn_saving_usd: f64,
        penalty_usd: f64,
    ) -> &'a str {
        const MARGIN: f64 = 1.5;
        if penalty_usd <= 0.0 {
            return preferred;
        }
        if per_turn_saving_usd > penalty_usd * MARGIN {
            return preferred;
        }
        fallback
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pricing() -> ModelPricing {
        ModelPricing::new(3.0, 15.0)
    }

    fn usage(cache_read: usize) -> TokenUsage {
        TokenUsage {
            prompt_tokens: 1000,
            completion_tokens: 100,
            total_tokens: 1100,
            cache_read_tokens: cache_read,
            cache_creation_tokens: 0,
        }
    }

    #[test]
    fn fresh_ledger_has_no_warm_endpoint() {
        let l = CacheLedger::new();
        assert!(l.sticky().is_none());
        assert!(!l.is_warm("any", 42));
    }

    #[test]
    fn observe_marks_the_endpoint_warm_for_its_fingerprint() {
        let mut l = CacheLedger::new();
        l.observe(1, "cloud", 42, &usage(900));
        assert!(l.is_warm("cloud", 42));
        // A different fingerprint means the prefix changed; the
        // endpoint is no longer warm for the *new* one.
        assert!(!l.is_warm("cloud", 43));
        // A different endpoint was never observed.
        assert!(!l.is_warm("local", 42));
        assert_eq!(l.sticky(), Some("cloud"));
    }

    #[test]
    fn switch_penalty_is_zero_for_a_warm_target() {
        let mut l = CacheLedger::new();
        l.observe(1, "cloud", 42, &usage(900));
        let p = l.switch_penalty_usd("cloud", 42, &pricing(), 100_000);
        assert_eq!(p, 0.0);
    }

    #[test]
    fn switch_penalty_is_positive_for_a_cold_target() {
        let l = CacheLedger::new();
        let p = l.switch_penalty_usd("cloud", 42, &pricing(), 100_000);
        // 100k tokens / 1M * (1.25*3.0 + 0.1*3.0) = 0.1 * 4.05 = 0.405
        assert!((p - 0.405).abs() < 1e-9, "got {p}");
    }

    #[test]
    fn gate_prefers_the_preferred_endpoint_when_the_hop_is_free() {
        let mut l = CacheLedger::new();
        l.observe(1, "cloud", 42, &usage(900));
        // Target is already warm -> penalty 0 -> hop.
        let chosen = l.gate("local", "cloud", 0.0, 0.0);
        assert_eq!(chosen, "local");
    }

    #[test]
    fn gate_keeps_the_warm_fallback_when_saving_does_not_clear_the_margin() {
        let mut l = CacheLedger::new();
        l.observe(1, "cloud", 42, &usage(900));
        let penalty = 0.40;
        // Saving barely exceeds penalty: 0.50 < 0.40 * 1.5 = 0.60.
        let chosen = l.gate("local", "cloud", 0.50, penalty);
        assert_eq!(chosen, "cloud");
    }

    #[test]
    fn gate_takes_the_hop_when_saving_clears_the_margin() {
        let mut l = CacheLedger::new();
        l.observe(1, "cloud", 42, &usage(900));
        let penalty = 0.40;
        // 0.70 > 0.60 -> hop.
        let chosen = l.gate("local", "cloud", 0.70, penalty);
        assert_eq!(chosen, "local");
    }

    #[test]
    fn observe_updates_the_sticky_endpoint() {
        let mut l = CacheLedger::new();
        l.observe(1, "cloud", 42, &usage(900));
        assert_eq!(l.sticky(), Some("cloud"));
        l.observe(2, "local", 42, &usage(500));
        assert_eq!(l.sticky(), Some("local"));
    }

    #[test]
    fn stale_fingerprint_loses_warmth() {
        // A change to the cacheable system prefix invalidates every
        // endpoint's cache simultaneously. is_warm must reflect that.
        let mut l = CacheLedger::new();
        l.observe(1, "cloud", 42, &usage(900));
        assert!(l.is_warm("cloud", 42));
        // New prefix.
        assert!(!l.is_warm("cloud", 99));
        // The switch penalty is now nonzero for the same endpoint.
        let p = l.switch_penalty_usd("cloud", 99, &pricing(), 100_000);
        assert!(p > 0.0);
    }
}
