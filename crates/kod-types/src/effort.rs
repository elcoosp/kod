//! Reasoning effort.
//!
//! A request can ask a provider to reason more or less. The level is a
//! property of the *request*, not of any provider or config, so it
//! lives here where both `kod-config` (the swarm's per-role default)
//! and `kod-provider` (the timeout it implies) can name it without
//! depending on each other.
//!
//! Ordered weakest to strongest: the `Ord` derive is the strength
//! order, which a timeout multiplier relies on.

use serde::{Deserialize, Serialize};

/// How much reasoning effort a request asks for.
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
    /// Parse a provider's spelling. Unknown values are `Medium` — a
    /// value kod does not recognise is more likely a new tier than a
    /// mistake, and the neutral default is the safe reading.
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

    /// The idle-timeout multiplier for this effort.
    ///
    /// `Medium` is neutral — what a caller who set no effort gets — so
    /// the default changes nothing. Above it the multiplier grows;
    /// below it shrinks, so a `None`-effort request fails fast rather
    /// than waiting out a timeout sized for a thinking model.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_medium() {
        assert_eq!(EffortLevel::default(), EffortLevel::Medium);
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

    #[test]
    fn parsing_is_case_insensitive_and_accepts_aliases() {
        assert_eq!(EffortLevel::parse("MAX"), EffortLevel::Max);
        assert_eq!(EffortLevel::parse("x-high"), EffortLevel::Xhigh);
        assert_eq!(EffortLevel::parse("min"), EffortLevel::Minimal);
    }

    #[test]
    fn multipliers_are_monotone_and_medium_is_neutral() {
        assert_eq!(EffortLevel::Medium.timeout_multiplier(), 1.0);
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
    fn an_unknown_effort_is_medium() {
        assert_eq!(EffortLevel::parse("turbo"), EffortLevel::Medium);
    }
}
