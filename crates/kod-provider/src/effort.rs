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

pub use kod_types::effort::EffortLevel;

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



}
