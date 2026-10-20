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
    /// Parse a provider's spelling. Unknown values are `Medium`:
    /// whether the value is a new tier or a typo, the neutral
    /// default is the safe reading either way.
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "none" => Self::None,
            "minimal" | "min" => Self::Minimal,
            "low" => Self::Low,
            "medium" | "med" => Self::Medium,
            "high" => Self::High,
            "xhigh" | "x-high" | "extra-high" => Self::Xhigh,
            "max" => Self::Max,
            _ => {
                // T5-C40: a new tier and a typo both read safely as
                // Medium. Warn so the user sees the fallback.
                tracing::warn!(
                    value = %s,
                    "effort: unrecognized value; falling back to Medium",
                );
                Self::Medium
            }
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
            assert!(
                pair[0] < pair[1],
                "{:?} must sort before {:?}",
                pair[0],
                pair[1]
            );
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

    #[test]
    fn unknown_effort_maps_to_medium() {
        // T5-C40: the fallback stays for forward-compat, but the
        // parse must not panic and must land on Medium.
        assert_eq!(EffortLevel::parse("turbo"), EffortLevel::Medium);
        assert_eq!(EffortLevel::parse("hight"), EffortLevel::Medium);
        assert_eq!(EffortLevel::parse(""), EffortLevel::Medium);
        assert_eq!(EffortLevel::parse("  "), EffortLevel::Medium);
    }
}

#[cfg(test)]
mod proptests {
    //! Properties for what the doc comments claim.
    //!
    //! The example tests in `tests` are exhaustive over the seven
    //! levels, so `Ord`, multiplier monotonicity and name round-trips
    //! are already covered for the *enum*. What they do not cover is
    //! `parse` over arbitrary *strings*: totality (never panics, always
    //! a level), the normalization contract (ASCII case and surrounding
    //! whitespace, and nothing else), idempotence of that normalization,
    //! and the known-alias / non-alias pair.
    //!
    //! The control in the non-alias property normalizes with a local
    //! `norm`, not `parse`, so the checker is not the code under test
    //! (L23). `norm` is ASCII-only on purpose: `parse` uses
    //! `to_ascii_lowercase`, and a control written with `to_lowercase`
    //! would disagree on `İ` / `K` (L46's trim trap, Unicode form).

    use super::EffortLevel;
    use proptest::prelude::*;
    use proptest::sample::select;

    /// Every level, weakest to strongest.
    const ALL: [EffortLevel; 7] = [
        EffortLevel::None,
        EffortLevel::Minimal,
        EffortLevel::Low,
        EffortLevel::Medium,
        EffortLevel::High,
        EffortLevel::Xhigh,
        EffortLevel::Max,
    ];

    /// Every spelling `parse` accepts, and the level it means. An
    /// alias added to `parse` without a row here is the drift this
    /// table exists to catch.
    const ALIASES: &[(&str, EffortLevel)] = &[
        ("none", EffortLevel::None),
        ("minimal", EffortLevel::Minimal),
        ("min", EffortLevel::Minimal),
        ("low", EffortLevel::Low),
        ("medium", EffortLevel::Medium),
        ("med", EffortLevel::Medium),
        ("high", EffortLevel::High),
        ("xhigh", EffortLevel::Xhigh),
        ("x-high", EffortLevel::Xhigh),
        ("extra-high", EffortLevel::Xhigh),
        ("max", EffortLevel::Max),
    ];

    /// The normalization `parse` claims: trim, then ASCII lowercase.
    /// Written independently of `parse` so the assertion cannot be
    /// satisfied by the code being wrong in the same way.
    fn norm(s: &str) -> String {
        s.trim().to_ascii_lowercase()
    }

    proptest! {
        /// `parse` is total: any string, no panic, always one of the
        /// seven levels.
        #[test]
        fn prop_parse_is_total(s in any::<String>()) {
            let l = EffortLevel::parse(&s);
            prop_assert!(ALL.contains(&l), "parse returned an out-of-range level");
        }

        /// Normalization is idempotent through the wire spelling.
        #[test]
        fn prop_parse_is_idempotent_through_as_str(s in any::<String>()) {
            let once = EffortLevel::parse(&s);
            prop_assert_eq!(EffortLevel::parse(once.as_str()), once);
        }

        /// Case and surrounding whitespace are ignored, and only
        /// ASCII case is folded.
        #[test]
        fn prop_parse_ignores_ascii_case_and_surrounding_whitespace(s in any::<String>()) {
            let l = EffortLevel::parse(&s);
            prop_assert_eq!(EffortLevel::parse(&s.to_ascii_lowercase()), l);
            prop_assert_eq!(EffortLevel::parse(s.trim()), l);
        }

        /// Pair A (L44): every accepted spelling maps to its level.
        #[test]
        fn prop_known_alias_maps_to_its_level((alias, want) in select(ALIASES)) {
            prop_assert_eq!(EffortLevel::parse(alias), want, "alias {:?}", alias);
        }

        /// Pair B (L44): a string that cannot be an alias maps to
        /// `Medium`. The alphabet is digits and punctuation, so no
        /// normalized value can equal an alias (all aliases are ASCII
        /// letters and `-`). The control asserts that, so the property
        /// is not vacuous.
        #[test]
        fn prop_non_alias_maps_to_medium(s in "[0-9!@#$%^&*()+=]{1,24}") {
            let n = norm(&s);
            prop_assume!(!ALIASES.iter().any(|(a, _)| norm(a) == n));
            prop_assert_eq!(EffortLevel::parse(&s), EffortLevel::Medium);
        }
    }

    /// Strict monotonicity, positivity, and finiteness of the timeout
    /// multiplier. `tests` asserts `<=`; the contract is that above
    /// Medium it *grows* and below it *shrinks*, which is strict.
    ///
    /// A plain `#[test]` outside the `proptest!` block on purpose: a
    /// bare `#[test]` inside it compiles to a confusing error on the
    /// wrong line (L49).
    #[test]
    fn timeout_multiplier_is_strictly_increasing_positive_and_finite() {
        for l in ALL {
            let m = l.timeout_multiplier();
            assert!(m > 0.0, "{l:?} multiplier must be positive, got {m}");
            assert!(m.is_finite(), "{l:?} multiplier must be finite, got {m}");
        }
        assert_eq!(EffortLevel::Medium.timeout_multiplier(), 1.0);
        for pair in ALL.windows(2) {
            assert!(
                pair[0].timeout_multiplier() < pair[1].timeout_multiplier(),
                "{:?} -> {:?} must strictly increase",
                pair[0],
                pair[1],
            );
        }
    }

    /// `as_str` (the provider wire spelling) and serde (the config
    /// spelling) must agree for every level. They are two
    /// independently maintained spellings; if they drift, a config
    /// file and a provider request disagree about what was asked for.
    #[test]
    fn as_str_matches_serde_serialization() {
        for l in ALL {
            let json = serde_json::to_value(l).expect("level serializes");
            assert_eq!(json, serde_json::json!(l.as_str()), "{l:?}");
        }
    }
}
