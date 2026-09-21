//! Endpoint circuit breaker (harness review section 9).
//!
//! A chronically failing primary endpoint is retried as primary on
//! every turn because `resolve_chain_for_task` rebuilds the chain
//! from scratch — it has no memory of the last turn's failures. A
//! provider that returns 500 for a minute, or a model that is not
//! loaded, costs the user one round of latency plus one error
//! surface per turn until they notice and switch manually.
//!
//! This module is a three-strikes breaker: after three consecutive
//! failures an endpoint is skipped in the chain for a cooldown
//! (default 60 seconds, configurable). A success clears the count.
//! A half-open probe (the first request after the cooldown) is
//! allowed through; if it succeeds the endpoint is healthy again,
//! if it fails the cooldown restarts.
//!
//! # Not a replacement for the fallback chain
//!
//! The existing A6 chain already walks to the next endpoint on a
//! retryable error within a single turn. This breaker is about
//! *across* turns: the primary that just failed three turns in a
//! row should not be tried first on the fourth.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// One endpoint's failure state.
#[derive(Debug, Clone)]
struct EndpointState {
    consecutive_failures: u32,
    /// When the cooldown expires; a caller checks this against now.
    /// `None` means the endpoint is healthy.
    cooldown_until: Option<Instant>,
    /// The last error string, for a UI's "why is this skipped".
    last_error: Option<String>,
}

/// The breaker. Cheap; one per engine behind an `Arc<Mutex<_>>`.
#[derive(Debug)]
pub struct EndpointHealth {
    states: HashMap<String, EndpointState>,
    /// Failure count that trips the breaker.
    strike_limit: u32,
    /// How long a tripped endpoint stays skipped.
    cooldown: Duration,
}

impl Default for EndpointHealth {
    fn default() -> Self {
        Self::new(3, Duration::from_secs(60))
    }
}

impl EndpointHealth {
    pub fn new(strike_limit: u32, cooldown: Duration) -> Self {
        Self {
            states: HashMap::new(),
            strike_limit: strike_limit.max(1),
            cooldown,
        }
    }

    /// Record a successful call: clear the failure count and any
    /// active cooldown.
    pub fn record_success(&mut self, endpoint: &str) {
        self.states.remove(endpoint);
    }

    /// Record a failure. Returns `true` if this failure *tripped*
    /// the breaker (crossed the strike limit for the first time),
    /// so a caller can log the transition.
    pub fn record_failure(&mut self, endpoint: &str, error: impl Into<String>) -> bool {
        let state = self
            .states
            .entry(endpoint.to_string())
            .or_insert(EndpointState {
                consecutive_failures: 0,
                cooldown_until: None,
                last_error: None,
            });
        state.consecutive_failures = state.consecutive_failures.saturating_add(1);
        state.last_error = Some(error.into());
        if state.consecutive_failures >= self.strike_limit && state.cooldown_until.is_none() {
            state.cooldown_until = Some(Instant::now() + self.cooldown);
            return true;
        }
        false
    }

    /// Whether the endpoint may be tried on this turn.
    ///
    /// Returns `true` when the endpoint is healthy or the cooldown
    /// has expired (a half-open probe). Returns `false` while the
    /// cooldown is active.
    ///
    /// A probe that fails again re-enters cooldown via
    /// `record_failure`; a probe that succeeds clears state via
    /// `record_success`.
    pub fn is_available(&mut self, endpoint: &str) -> bool {
        let Some(state) = self.states.get(endpoint) else {
            return true;
        };
        match state.cooldown_until {
            None => true,
            Some(until) if Instant::now() >= until => {
                // Cooldown expired; allow a probe. Drop the
                // cooldown so the next failure re-arms it, but keep
                // the failure count — the endpoint has not proven
                // itself yet.
                let s = self.states.get_mut(endpoint).unwrap();
                s.cooldown_until = None;
                true
            }
            Some(_) => false,
        }
    }

    /// Filter a chain by availability, preserving order. If the
    /// filter would empty the chain, the *first* entry is kept — a
    /// turn served by a cold endpoint is better than a turn with no
    /// endpoint. A caller that wants the strict behavior can
    /// pre-check `is_available` on each entry.
    pub fn filter_chain<T, F>(&mut self, chain: &[T], endpoint_of: F) -> Vec<T>
    where
        T: Clone,
        F: Fn(&T) -> &str,
    {
        let available: Vec<T> = chain
            .iter()
            .filter(|m| self.is_available(endpoint_of(m)))
            .cloned()
            .collect();
        if available.is_empty() && !chain.is_empty() {
            return vec![chain[0].clone()];
        }
        available
    }

    /// Last recorded error for an endpoint, for a `/cache` surface
    /// or an audit line.
    pub fn last_error(&self, endpoint: &str) -> Option<&str> {
        self.states.get(endpoint).and_then(|s| s.last_error.as_deref())
    }

    /// Snapshot of the failing endpoints, for a `/cache` or
    /// `/doctor` readout.
    pub fn unhealthy(&self) -> Vec<(String, u32, Option<&str>)> {
        let now = Instant::now();
        self.states
            .iter()
            .filter(|(_, s)| s.cooldown_until.map(|u| u > now).unwrap_or(false))
            .map(|(name, s)| (name.clone(), s.consecutive_failures, s.last_error.as_deref()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_endpoint_is_available() {
        let mut h = EndpointHealth::default();
        assert!(h.is_available("a"));
    }

    #[test]
    fn fewer_than_three_failures_does_not_trip() {
        let mut h = EndpointHealth::new(3, Duration::from_secs(60));
        h.record_failure("a", "boom");
        h.record_failure("a", "boom");
        assert!(h.is_available("a"));
    }

    #[test]
    fn third_failure_trips_and_skips() {
        let mut h = EndpointHealth::new(3, Duration::from_secs(60));
        assert!(!h.record_failure("a", "one"));
        assert!(!h.record_failure("a", "two"));
        assert!(h.record_failure("a", "three"), "third failure must trip");
        assert!(!h.is_available("a"));
    }

    #[test]
    fn a_success_clears_the_cooldown() {
        let mut h = EndpointHealth::new(3, Duration::from_secs(60));
        h.record_failure("a", "x");
        h.record_failure("a", "x");
        h.record_failure("a", "x");
        assert!(!h.is_available("a"));
        h.record_success("a");
        assert!(h.is_available("a"));
    }

    #[test]
    fn cooldown_expiry_allows_a_probe() {
        // Zero cooldown: the trip is immediate but so is expiry.
        let mut h = EndpointHealth::new(3, Duration::from_millis(0));
        h.record_failure("a", "x");
        h.record_failure("a", "x");
        h.record_failure("a", "x");
        std::thread::sleep(Duration::from_millis(5));
        assert!(h.is_available("a"), "expired cooldown must allow a probe");
    }

    #[test]
    fn filter_chain_drops_unavailable_endpoints() {
        let mut h = EndpointHealth::new(3, Duration::from_secs(60));
        for _ in 0..3 {
            h.record_failure("bad", "x");
        }
        let chain = vec!["bad", "good", "other"];
        let filtered = h.filter_chain(&chain, |s| s);
        assert_eq!(filtered, vec!["good", "other"]);
    }

    #[test]
    fn filter_chain_keeps_the_first_when_all_are_cooling() {
        let mut h = EndpointHealth::new(3, Duration::from_secs(60));
        for e in ["a", "b"] {
            for _ in 0..3 {
                h.record_failure(e, "x");
            }
        }
        let chain = vec!["a", "b"];
        let filtered = h.filter_chain(&chain, |s| s);
        assert_eq!(filtered, vec!["a"], "must fall back to the first entry");
    }

    #[test]
    fn last_error_is_recorded() {
        let mut h = EndpointHealth::default();
        h.record_failure("a", "connection refused");
        assert_eq!(h.last_error("a"), Some("connection refused"));
    }

    #[test]
    fn unhealthy_lists_only_cooling_endpoints() {
        let mut h = EndpointHealth::new(3, Duration::from_secs(60));
        for _ in 0..3 {
            h.record_failure("bad", "x");
        }
        h.record_failure("fine", "once");
        let u = h.unhealthy();
        assert_eq!(u.len(), 1);
        assert_eq!(u[0].0, "bad");
    }
}
