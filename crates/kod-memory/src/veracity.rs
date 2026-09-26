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
        for v in [Veracity::Inferred, Veracity::Imported, Veracity::Unknown, Veracity::Tool] {
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
        assert_eq!(base_confidence(Veracity::Stated), Veracity::Stated.base_confidence());
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
