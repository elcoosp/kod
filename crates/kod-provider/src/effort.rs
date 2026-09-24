//! Reasoning effort and the idle timeouts it implies.
//!
//! A reasoning model produces no output while it thinks. The gap is
//! legitimate — a `Max`-effort model working a hard problem can be
//! silent for minutes — but from the outside it is indistinguishable
//! from a wedged connection. The fixed idle timeout that is correct
//! for a fast model kills a slow one mid-thought; the timeout that
//! spares the slow model waits forever on a genuinely hung fast one.
//!
//! This module scales the idle timeout by the effort the request
//! asked for, so the slow model gets the time it needs and the fast
//! model still fails fast. It is a pure function of `(base, effort)`;
//! a caller that never sets an effort gets the base unchanged.

use serde::{Deserialize, Serialize};

/// How much reasoning effort a request asks for.
///
/// Ordered weakest to strongest: the `Ord` derive is the strength
/// order, which the timeout multiplier relies on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum EffortLevel {
    /// No reasoning: a direct answer.
    None,
    /// A little: a short chain.
    Minimal,
    Low,
    /// The default. A model that thinks briefly.
    #[default]
    Medium,
    High,
    /// Above high: for a hard problem where the answer matters more
    /// than the latency.
    Xhigh,
    /// The maximum the provider offers. Minutes of silence are normal.
    Max,
}

impl EffortLevel {
    /// The idle-timeout multiplier for this effort.
    ///
    /// A `Medium` request uses the base unchanged — that is what a
    /// caller who set no effort gets, so the default is neutral. Above
    /// it the multiplier grows; below it shrinks, so a `None`-effort
    /// request fails fast rather than waiting out a timeout sized for
    /// a thinking model.
    ///
    /// The progression is roughly exponential: 180s base becomes 360s
    /// at `High`, 540s at `Xhigh`, 720s at `Max`. Those are the
    /// notebook's numbers — a reasoning model's silence scales with
    /// how much it was asked to reason.
    pub fn timeout_multiplier(self) -> f32 {
        match self {
            Self::None => 0.5,
            Self::Minimal => 0.75,
            Self::Low => 0.9,
            Self::Medium => 1.0,
            Self::High => 2.0,
            Self::Xhigh => 3.0,
            Self::Max => 4.0,
        }
    }

    /// Parse a provider's spelling. Unknown values are `Medium` — a
    /// value kod does not recognise is more likely a new tier than a
    /// mistake, and defaulting to the neutral multiplier is the safe
    /// reading.
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "none" => Self::None,
            "minimal" | "min" => Self::Minimal,
            "low" => Self::Low,
            "medium" | "med" => Self::Medium,
            "high" => Self::High,
            "xhigh" | "x-high" | "extra-high" => Self::Xhigh,
            "max" => Self::Max,
            _ => Self::Medium,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }
}

/// Scale an idle timeout by the effort a request asked for.
///
/// A zero base stays zero: a caller that disabled the timeout meant
/// it, and scaling zero would silently re-enable it.
pub fn scaled_idle_timeout(base: std::time::Duration, effort: EffortLevel) -> std::time::Duration {
    if base.is_zero() {
        return base;
    }
    base.mul_f32(effort.timeout_multiplier())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn medium_is_the_neutral_multiplier() {
        assert_eq!(EffortLevel::Medium.timeout_multiplier(), 1.0);
        let base = Duration::from_secs(180);
        assert_eq!(scaled_idle_timeout(base, EffortLevel::Medium), base);
    }

    #[test]
    fn default_is_medium() {
        assert_eq!(EffortLevel::default(), EffortLevel::Medium);
    }

    #[test]
    fn higher_effort_gets_more_time() {
        let base = Duration::from_secs(180);
        let medium = scaled_idle_timeout(base, EffortLevel::Medium);
        let high = scaled_idle_timeout(base, EffortLevel::High);
        let max = scaled_idle_timeout(base, EffortLevel::Max);
        assert!(high > medium);
        assert!(max > high);
        assert_eq!(max, Duration::from_secs(720));
    }

    #[test]
    fn lower_effort_fails_faster() {
        let base = Duration::from_secs(180);
        let none = scaled_idle_timeout(base, EffortLevel::None);
        assert!(none < base, "a no-reasoning request should not wait out a thinking timeout");
    }

    #[test]
    fn ordering_is_weakest_to_strongest() {
        let levels = [
            EffortLevel::None,
            EffortLevel::Minimal,
            EffortLevel::Low,
            EffortLevel::Medium,
            EffortLevel::High,
            EffortLevel::Xhigh,
            EffortLevel::Max,
        ];
        for pair in levels.windows(2) {
            assert!(pair[0] < pair[1], "{:?} must sort before {:?}", pair[0], pair[1]);
        }
    }

    #[test]
    fn multipliers_are_monotone() {
        let levels = [
            EffortLevel::None,
            EffortLevel::Minimal,
            EffortLevel::Low,
            EffortLevel::Medium,
            EffortLevel::High,
            EffortLevel::Xhigh,
            EffortLevel::Max,
        ];
        for pair in levels.windows(2) {
            assert!(
                pair[0].timeout_multiplier() <= pair[1].timeout_multiplier(),
                "{:?} -> {:?} must not decrease",
                pair[0],
                pair[1],
            );
        }
    }

    #[test]
    fn a_zero_base_stays_zero() {
        // A caller that disabled the timeout meant it.
        assert_eq!(scaled_idle_timeout(Duration::ZERO, EffortLevel::Max), Duration::ZERO);
    }

    #[test]
    fn parsing_is_case_insensitive_and_accepts_aliases() {
        assert_eq!(EffortLevel::parse("MAX"), EffortLevel::Max);
        assert_eq!(EffortLevel::parse("x-high"), EffortLevel::Xhigh);
        assert_eq!(EffortLevel::parse("min"), EffortLevel::Minimal);
    }

    #[test]
    fn an_unknown_effort_is_medium() {
        // A value kod does not know is more likely a new tier than a
        // mistake; the neutral multiplier is the safe reading.
        assert_eq!(EffortLevel::parse("turbo"), EffortLevel::Medium);
    }

    #[test]
    fn every_level_round_trips_through_its_name() {
        for l in [
            EffortLevel::None,
            EffortLevel::Minimal,
            EffortLevel::Low,
            EffortLevel::Medium,
            EffortLevel::High,
            EffortLevel::Xhigh,
            EffortLevel::Max,
        ] {
            assert_eq!(EffortLevel::parse(l.as_str()), l);
        }
    }
}
