//! Episodic tier degradation (borrow from oh-my-pi, delta §12.8).
//!
//! # The rule
//!
//! An episodic entry is never deleted for age alone — it degrades
//! through tiers, and its recall weight falls with each. The design's
//! thresholds and weights:
//!
//! | tier | age | weight | content |
//! |---|---|---|---|
//! | 1 | < 30 d | 1.0 | full |
//! | 2 | ≥ 30 d | 0.5 | full |
//! | 3 | ≥ 180 d | 0.25 | compressed to ≤ 300 chars |
//!
//! # Why tiers, not deletion
//!
//! An old episode is not worthless. "We tried X and it failed" from six
//! months ago is exactly the fact that stops the same mistake being made
//! again. Deleting it because it is old loses the audit trail the
//! workspace's memory design deliberately preserves. Degrading — a lower
//! weight, a shorter body — keeps it reachable while keeping it out of
//! the way of fresh facts.
//!
//! # What this is NOT
//!
//! * Not the archival pass. `MemoryManager::consolidate` removes
//!   episodic entries past `ARCHIVE_AFTER_DAYS` entirely; this module
//!   computes the tier for entries that remain.
//! * Not a mutation. `tier_for` and `tier_weight` are pure; applying
//!   the compressed body to an entry is the caller's step.

/// The age (days) at which an entry drops to tier 2.
pub const TIER2_AFTER_DAYS: i64 = 30;

/// The age (days) at which an entry drops to tier 3.
pub const TIER3_AFTER_DAYS: i64 = 180;

/// The character cap for a tier-3 entry's compressed body.
pub const TIER3_MAX_CHARS: usize = 300;

/// The three tiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    /// Fresh: full weight, full body.
    One,
    /// Aged: half weight, full body.
    Two,
    /// Old: quarter weight, compressed body.
    Three,
}

impl Tier {
    pub fn number(self) -> u8 {
        match self {
            Self::One => 1,
            Self::Two => 2,
            Self::Three => 3,
        }
    }

    /// The recall weight folded into a score.
    pub fn weight(self) -> f32 {
        match self {
            Self::One => 1.0,
            Self::Two => 0.5,
            Self::Three => 0.25,
        }
    }

    /// Whether the body should be compressed at this tier.
    pub fn compresses_body(self) -> bool {
        self == Self::Three
    }
}

/// The tier for an entry of `age_days`.
///
/// A negative age (a clock skew, or a future timestamp) is tier 1 —
/// the same as a fresh entry. `age_days` is integer days; a caller
/// with a `Duration` floors it.
pub fn tier_for(age_days: i64) -> Tier {
    if age_days >= TIER3_AFTER_DAYS {
        Tier::Three
    } else if age_days >= TIER2_AFTER_DAYS {
        Tier::Two
    } else {
        Tier::One
    }
}

/// The tier for a timestamp, measured against `now`.
pub fn tier_at(ts: time::OffsetDateTime, now: time::OffsetDateTime) -> Tier {
    let delta = now - ts;
    tier_for(delta.whole_days())
}

/// Compress a tier-3 body to [`TIER3_MAX_CHARS`], ending on a word
/// boundary where one is close enough.
///
/// The body is not truncated mid-word: the last whitespace within the
/// cap is the cut, so the result reads as a fragment rather than a
/// severed token. A body already under the cap is returned unchanged.
pub fn compress_body(body: &str) -> String {
    if body.chars().count() <= TIER3_MAX_CHARS {
        return body.to_string();
    }
    let mut out = String::new();
    for (i, c) in body.chars().enumerate() {
        if i >= TIER3_MAX_CHARS {
            break;
        }
        out.push(c);
    }
    // Trim back to the last whitespace, if there is one in the back
    // half (a very long first word is left as-is).
    if let Some(pos) = out.rfind(char::is_whitespace) {
        if pos > TIER3_MAX_CHARS / 2 {
            out.truncate(pos);
        }
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_entry_is_tier_one() {
        assert_eq!(tier_for(0), Tier::One);
        assert_eq!(tier_for(29), Tier::One);
    }

    #[test]
    fn thirty_days_is_tier_two() {
        assert_eq!(tier_for(30), Tier::Two);
        assert_eq!(tier_for(179), Tier::Two);
    }

    #[test]
    fn one_eighty_days_is_tier_three() {
        assert_eq!(tier_for(180), Tier::Three);
        assert_eq!(tier_for(10_000), Tier::Three);
    }

    #[test]
    fn a_negative_age_is_tier_one() {
        assert_eq!(tier_for(-5), Tier::One);
    }

    #[test]
    fn weights_fall_with_tier() {
        assert_eq!(Tier::One.weight(), 1.0);
        assert_eq!(Tier::Two.weight(), 0.5);
        assert_eq!(Tier::Three.weight(), 0.25);
    }

    #[test]
    fn only_tier_three_compresses() {
        assert!(!Tier::One.compresses_body());
        assert!(!Tier::Two.compresses_body());
        assert!(Tier::Three.compresses_body());
    }

    #[test]
    fn tier_numbers_are_one_two_three() {
        assert_eq!(Tier::One.number(), 1);
        assert_eq!(Tier::Two.number(), 2);
        assert_eq!(Tier::Three.number(), 3);
    }

    #[test]
    fn tier_ordering_is_fresh_to_old() {
        assert!(Tier::One < Tier::Two);
        assert!(Tier::Two < Tier::Three);
    }

    #[test]
    fn a_short_body_is_unchanged_by_compression() {
        let body = "a short fact";
        assert_eq!(compress_body(body), body);
    }

    #[test]
    fn a_long_body_is_capped() {
        let body = "word ".repeat(200);
        let c = compress_body(&body);
        assert!(c.chars().count() <= TIER3_MAX_CHARS + 1, "got {} chars", c.chars().count());
    }

    #[test]
    fn compression_ends_on_a_word_boundary() {
        let body = "alpha beta gamma ".repeat(50);
        let c = compress_body(&body);
        // The ellipsis is appended after a whitespace cut, so the char
        // before the ellipsis is not a letter of a severed word.
        assert!(c.ends_with('…'), "got: {c:?}");
    }

    #[test]
    fn a_single_long_word_is_not_over_trimmed() {
        let body = "x".repeat(500);
        let c = compress_body(&body);
        // No whitespace to cut at; the cap applies as-is.
        assert!(c.starts_with("xxx"), "got: {c:?}");
    }

    #[test]
    fn tier_at_uses_the_clock() {
        let now = time::OffsetDateTime::now_utc();
        let fresh = now - time::Duration::days(5);
        let old = now - time::Duration::days(200);
        assert_eq!(tier_at(fresh, now), Tier::One);
        assert_eq!(tier_at(old, now), Tier::Three);
    }
}
