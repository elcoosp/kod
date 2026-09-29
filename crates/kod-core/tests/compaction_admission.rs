//! Delta §4.3: compaction admission control (no-reduction guard).
//!
//! The design's rule: reject a compaction plan whose projected
//! post-state is not smaller than the pre-state, measured locally
//! with the same estimator the planner used. A summarizer asked to
//! reduce a transcript can, in the worst case, produce a summary
//! longer than what it replaces; the guard catches that before the
//! transcript is drained.
//!
//! # The reserve is not an admission floor
//!
//! The design also names `reserve = max(15% of window, 16_384)` and
//! a `projected <= window - reserve` projection. That rule is the
//! `should_compact` trigger — the threshold that decides *when to
//! try* — not the admission rule. Conflating them breaks small
//! windows: with `window = 8_192` and `reserve = 16_384` the
//! projection is `projected <= 0` and every plan is rejected,
//! including a handoff that reduces a 7 000-token transcript to a
//! 50-token summary. The reserve is kept on the struct for a caller
//! that wants the projection check via `fits_under_reserve`.

use kod_core::compaction_dispatcher::{CompactionAdmission, resolve_reserve};

#[test]
fn growth_is_rejected() {
    let a = CompactionAdmission {
        projected: 200_000,
        current: 180_000,
        window: 200_000,
        reserve: resolve_reserve(200_000),
    };
    assert!(
        !a.admits(),
        "a summary longer than the transcript is not a reduction",
    );
}

#[test]
fn a_shrinking_plan_is_admitted() {
    let a = CompactionAdmission {
        projected: 5_000,
        current: 180_000,
        window: 200_000,
        reserve: resolve_reserve(200_000),
    };
    assert!(a.admits());
}

#[test]
fn a_small_window_still_admits_a_shrinking_plan() {
    // Regression: the pre-fix reserve-as-floor logic rejected every
    // plan when window < reserve.
    let a = CompactionAdmission {
        projected: 50,
        current: 7_000,
        window: 8_192,
        reserve: resolve_reserve(8_192),
    };
    assert!(a.admits());
}

#[test]
fn the_reserve_check_is_a_separate_predicate() {
    let a = CompactionAdmission {
        projected: 175_000,
        current: 180_000,
        window: 200_000,
        reserve: resolve_reserve(200_000),
    };
    assert!(a.admits(), "a shrink is a shrink");
    assert!(!a.fits_under_reserve(), "but the reserve is breached");
}

#[test]
fn the_reserve_matches_the_dispatcher_rule() {
    assert_eq!(resolve_reserve(50_000), 16_384);
    assert_eq!(resolve_reserve(200_000), 30_000);
}
