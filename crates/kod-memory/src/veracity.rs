//! Veracity consolidation primitives (borrow from oh-my-pi, delta
//! §12.2).
//!
//! # The problem
//!
//! A memory store accumulates the same fact stated several ways, and
//! facts that contradict. The design's answer is *confidence*: a fact
//! starts at a base confidence set by how it was learned, and each
//! re-mention raises it with a saturating update. A fact whose
//! confidence is high enough supersedes one it contradicts.
//!
//! # The three pieces
//!
//! * [`fact_content_id`] — a content address for a
//!   subject-predicate-object triple.
//! * [`Veracity`] — how a fact was learned, mapped to a weight.
//! * [`raise_confidence`] / [`base_confidence`] — the saturating
//!   update and the starting value.
//!
//! # The triple gap
//!
//! [`crate::extract::ExtractedFact`] is `{ content, kind }` — a
//! single string, not a parsed triple. [`fact_content_id`] therefore
//! takes three parts that the current extractor does not produce.
//! Wiring the content id to real facts needs the extraction pass to
//! emit a subject-predicate-object split, which is a larger change
//! than this module. The primitive stands on its own: a caller that
//! has a triple — a future extractor, or a hand-authored fact — can
//! address it.
//!
//! # What this is NOT
//!
//! * Not the triple extractor.
//! * Not the consolidation loop. Resolving a contradiction by picking
//!   the higher-confidence side is `MemoryManager::consolidate`'s job.

/// How a fact was learned. Ordered weakest to strongest evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Veracity {
    Stated,
    Inferred,
    Imported,
    Unknown,
    Tool,
}

impl Veracity {
    /// The design's evidence weight for this source.
    pub fn weight(self) -> f64 {
        match self {
            Self::Stated => 1.0,
            Self::Inferred => 0.7,
            Self::Imported => 0.6,
            Self::Unknown => 0.8,
            Self::Tool => 0.5,
        }
    }

    /// The base confidence a brand-new fact starts at:
    /// `weight * 0.5`.
    pub fn base_confidence(self) -> f64 {
        self.weight() * 0.5
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stated => "stated",
            Self::Inferred => "inferred",
            Self::Imported => "imported",
            Self::Unknown => "unknown",
            Self::Tool => "tool",
        }
    }
}

/// The re-mention increment coefficient. The design's 0.3.
pub const MENTION_STEP: f64 = 0.3;

/// A content address for a subject-predicate-object triple:
/// `cf_` plus the first 12 bytes of SHA-256 over the length-prefixed
/// parts. Length-prefixing means `("ab","c")` and `("a","bc")` get
/// different ids.
///
/// # NFC
///
/// The design calls for Unicode NFC before hashing. kod has no
/// normalisation crate and the facts are overwhelmingly ASCII, so
/// this hashes as-is and documents the gap: a combining accent and a
/// precomposed character would produce two ids. `unicode-normalization`
/// closes it; not worth the dependency for ASCII.
pub fn fact_content_id(subject: &str, predicate: &str, object: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    for part in [subject, predicate, object] {
        h.update((part.len() as u64).to_le_bytes());
        h.update(part.as_bytes());
    }
    let digest = h.finalize();
    let hex: String = digest[..12].iter().map(|b| format!("{b:02x}")).collect();
    format!("cf_{hex}")
}

/// Raise a fact's confidence on a re-mention:
/// `conf += (1 - conf) * weight * 0.3`, clamped to `[0, 1]`.
pub fn raise_confidence(confidence: f64, weight: f64) -> f64 {
    let c = confidence.clamp(0.0, 1.0);
    (c + (1.0 - c) * weight * MENTION_STEP).clamp(0.0, 1.0)
}

/// The base confidence a new fact starts at, given its source.
pub fn base_confidence(source: Veracity) -> f64 {
    source.base_confidence()
}

/// Whether `challenger` should supersede `incumbent` on a
/// contradiction: true only when strictly higher. A tie keeps the
/// incumbent — the design surfaces a contradiction rather than
/// silently picking a winner, and a tie has no winner.
pub fn should_supersede(challenger: f64, incumbent: f64) -> bool {
    challenger > incumbent
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_content_id_is_stable() {
        assert_eq!(
            fact_content_id("user", "prefers", "tabs"),
            fact_content_id("user", "prefers", "tabs"),
        );
    }

    #[test]
    fn a_content_id_has_the_documented_shape() {
        let id = fact_content_id("s", "p", "o");
        assert!(id.starts_with("cf_"), "got: {id}");
        assert_eq!(id.len(), 3 + 24, "cf_ plus 24 hex; got {id}");
    }

    #[test]
    fn different_triples_get_different_ids() {
        assert_ne!(
            fact_content_id("user", "prefers", "tabs"),
            fact_content_id("user", "prefers", "spaces"),
        );
    }

    #[test]
    fn length_prefixing_prevents_a_join_collision() {
        assert_ne!(
            fact_content_id("ab", "c", "d"),
            fact_content_id("a", "bc", "d"),
        );
    }

    #[test]
    fn the_weights_match_the_design_table() {
        assert_eq!(Veracity::Stated.weight(), 1.0);
        assert_eq!(Veracity::Inferred.weight(), 0.7);
        assert_eq!(Veracity::Imported.weight(), 0.6);
        assert_eq!(Veracity::Unknown.weight(), 0.8);
        assert_eq!(Veracity::Tool.weight(), 0.5);
    }

    #[test]
    fn stated_is_the_strongest() {
        for v in [
            Veracity::Inferred,
            Veracity::Imported,
            Veracity::Unknown,
            Veracity::Tool,
        ] {
            assert!(v.weight() < Veracity::Stated.weight());
        }
    }

    #[test]
    fn a_stated_fact_starts_at_a_half() {
        assert_eq!(Veracity::Stated.base_confidence(), 0.5);
    }

    #[test]
    fn an_inferred_fact_starts_lower() {
        assert!((Veracity::Inferred.base_confidence() - 0.35).abs() < 1e-9);
    }

    #[test]
    fn a_re_mention_raises_confidence() {
        let c = raise_confidence(0.5, 1.0);
        assert!((c - 0.65).abs() < 1e-9, "got {c}");
    }

    #[test]
    fn confidence_saturates_at_one() {
        let mut c = 0.9;
        for _ in 0..100 {
            c = raise_confidence(c, 1.0);
        }
        assert!((c - 1.0).abs() < 1e-9, "got {c}");
    }

    #[test]
    fn a_weaker_source_raises_less() {
        assert!(
            raise_confidence(0.5, Veracity::Stated.weight())
                > raise_confidence(0.5, Veracity::Tool.weight())
        );
    }

    #[test]
    fn confidence_never_exceeds_one() {
        assert_eq!(raise_confidence(2.0, 1.0), 1.0);
    }

    #[test]
    fn base_confidence_fn_matches_the_enum() {
        assert_eq!(
            base_confidence(Veracity::Stated),
            Veracity::Stated.base_confidence()
        );
    }

    #[test]
    fn a_higher_confidence_challenger_supersedes() {
        assert!(should_supersede(0.8, 0.5));
    }

    #[test]
    fn a_lower_confidence_challenger_does_not() {
        assert!(!should_supersede(0.3, 0.5));
    }

    #[test]
    fn a_tie_keeps_the_incumbent() {
        assert!(!should_supersede(0.5, 0.5));
    }

    #[test]
    fn veracity_as_str_is_the_documented_spelling() {
        assert_eq!(Veracity::Stated.as_str(), "stated");
        assert_eq!(Veracity::Tool.as_str(), "tool");
    }
}

#[cfg(test)]
mod veracity_proptests {
    //! Properties for the numeric contracts in `veracity`.
    //!
    //! The 17 example tests pin discrete points: a re-mention
    //! raises confidence, confidence saturates at one, a stated
    //! fact starts at a half. The properties below pin the
    //! *continuous-domain* shape: monotone in each argument,
    //! bounded, fixed-point structure. A future change to
    //! `raise_confidence`'s formula that broke monotonicity would
    //! pass every example test and fail here.
    //!
    //! `should_supersede` is `>` on `f64`. Three properties pin
    //! that: strict (no self-supersede), antisymmetric, and
    //! equivalent to `>` for finite inputs.
    //!
    //! **No bug expected.** The bodies are three-liners with
    //! `clamp` on both ends. Two controls pin the NaN edge (NaN
    //! propagates through `clamp`, and NaN comparisons are false),
    //! which the current code handles by never superseding and by
    //! returning NaN — the caller's production inputs are finite,
    //! so NaN is unreachable, but the contract is worth pinning.

    use super::*;
    use proptest::prelude::*;

    fn finite_unit() -> impl Strategy<Value = f64> {
        (0.0f64..=1.0f64)
    }

    proptest! {
        /// `raise_confidence` is monotone non-decreasing in
        /// `confidence` for a fixed weight.
        #[test]
        fn prop_raise_monotone_in_confidence(
            c1 in finite_unit(),
            c2 in finite_unit(),
            w in finite_unit(),
        ) {
            let (lo, hi) = if c1 <= c2 { (c1, c2) } else { (c2, c1) };
            let a = raise_confidence(lo, w);
            let b = raise_confidence(hi, w);
            prop_assert!(
                a <= b,
                "raise({}, {}) = {} > raise({}, {}) = {}",
                lo, w, a, hi, w, b,
            );
        }

        /// `raise_confidence` is monotone non-decreasing in weight
        /// for a fixed confidence. At `c = 1.0` it is flat: any
        /// weight gives 1.0 (the saturation case).
        #[test]
        fn prop_raise_monotone_in_weight(
            c in finite_unit(),
            w1 in finite_unit(),
            w2 in finite_unit(),
        ) {
            let (lo, hi) = if w1 <= w2 { (w1, w2) } else { (w2, w1) };
            let a = raise_confidence(c, lo);
            let b = raise_confidence(c, hi);
            prop_assert!(
                a <= b,
                "raise({}, {}) = {} > raise({}, {}) = {}",
                c, lo, a, c, hi, b,
            );
        }

        /// The output is in `[0, 1]` for finite input. Both inputs
        /// are clamped, so the output cannot escape.
        #[test]
        fn prop_raise_bounded(
            c in any::<f64>(),
            w in any::<f64>(),
        ) {
            prop_assume!(c.is_finite() && w.is_finite());
            let out = raise_confidence(c, w);
            prop_assert!(
                (0.0..=1.0).contains(&out),
                "raise({}, {}) = {} not in [0, 1]", c, w, out,
            );
        }

        /// `raise_confidence` never decreases a confidence in
        /// `[0, 1]` for a non-negative weight.
        #[test]
        fn prop_raise_never_decreases(
            c in finite_unit(),
            w in 0.0f64..=1.0f64,
        ) {
            let out = raise_confidence(c, w);
            prop_assert!(
                out >= c,
                "raise({}, {}) = {} < {}", c, w, out, c,
            );
        }

        /// `raise_confidence` saturates at 1.0 for any weight.
        #[test]
        fn prop_raise_saturates_at_one(w in finite_unit()) {
            prop_assert_eq!(raise_confidence(1.0, w), 1.0, "w = {}", w);
        }

        /// A zero weight is a fixed point.
        #[test]
        fn prop_raise_zero_weight_fixed_point(c in finite_unit()) {
            prop_assert_eq!(raise_confidence(c, 0.0), c, "c = {}", c);
        }

        /// `should_supersede` is strictly greater: no value
        /// supersedes itself, and a tie keeps the incumbent (the
        /// doc comment says so explicitly).
        #[test]
        fn prop_supersede_strict(x in any::<f64>()) {
            prop_assume!(x.is_finite());
            prop_assert!(!should_supersede(x, x), "x = {}", x);
        }

        /// Antisymmetric: at most one direction can supersede.
        #[test]
        fn prop_supersede_antisymmetric(
            challenger in any::<f64>(),
            incumbent in any::<f64>(),
        ) {
            prop_assume!(challenger.is_finite() && incumbent.is_finite());
            let a = should_supersede(challenger, incumbent);
            let b = should_supersede(incumbent, challenger);
            prop_assert!(!(a && b), "both directions superseded: {} vs {}", challenger, incumbent);
        }

        /// Equivalent to `>` for finite inputs. The body is `>`,
        /// so this is a tautology today; it exists so a change
        /// from `>` to `>=` (or to a tolerance-based comparison)
        /// fails here.
        #[test]
        fn prop_supersede_equivalent_to_gt(
            challenger in any::<f64>(),
            incumbent in any::<f64>(),
        ) {
            prop_assume!(challenger.is_finite() && incumbent.is_finite());
            prop_assert_eq!(
                should_supersede(challenger, incumbent),
                challenger > incumbent,
            );
        }
    }

    // ---- controls: NaN edges -------------------------------------

    /// `should_supersede(NaN, x)` is false for any `x`, because
    /// `NaN > x` is false in IEEE 754. The production callers
    /// pass finite confidences, so NaN is unreachable; pinned so a
    /// future `partial_cmp` + `Ordering::Greater` refactor (which
    /// would return `None` and might panic on `unwrap`) is a
    /// visible decision.
    #[test]
    fn control_nan_challenger_never_supersedes() {
        assert!(!should_supersede(f64::NAN, 0.5));
        assert!(!should_supersede(f64::NAN, 0.0));
        assert!(!should_supersede(f64::NAN, 1.0));
    }

    /// `raise_confidence(NaN, w)` is NaN. `f64::clamp` panics when
    /// min > max, and returns NaN when the value is NaN — so the
    /// output is NaN. Production inputs are finite (memory entries
    /// store finite f32 confidences; the weights are a fixed f64
    /// table); pinned so a future "sanitize NaN to 0" (or a
    /// `debug_assert` on finiteness) is a visible decision.
    #[test]
    fn control_nan_confidence_propagates() {
        assert!(raise_confidence(f64::NAN, 0.3).is_nan());
        assert!(raise_confidence(f64::NAN, 0.0).is_nan());
    }
}
