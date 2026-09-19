//! Session cost accumulator (Tier 1.2).
//!
//! One place that knows how much the session has spent and how much
//! of the caps is used. Updated synchronously by `record_cost`; the
//! TUI reads it on every render.
//!
//! Integer micro-USD arithmetic avoids float drift over a long
//! session: $0.000001 is one unit, so a session capped at $5.00 fits
//! comfortably in a u64.

use kod_config::limits::OnExhausted;
use parking_lot::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};

/// A snapshot of the current spend state. `Copy` so a UI can hold it.
#[derive(Debug, Clone, Copy, Default)]
pub struct CostSnapshot {
    pub session_usd: f64,
    pub session_cap_usd: f64,
    pub turn_usd: f64,
    pub turn_cap_usd: f64,
    pub session_fraction: f64,
    pub turn_fraction: f64,
    pub session_warned: bool,
    pub turn_warned: bool,
    pub exhausted: bool,
}

/// Which cap tripped a soft warning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoftWarningTrigger {
    Session,
    Turn,
}

/// The live accumulator. Cloning shares state.
#[derive(Clone)]
pub struct CostTracker {
    inner: std::sync::Arc<CostInner>,
}

struct CostInner {
    session_micro: AtomicU64,
    turn_micro: AtomicU64,
    session_cap_micro: AtomicU64,
    turn_cap_micro: AtomicU64,
    soft_warn_milli: AtomicU64,
    session_warned: RwLock<bool>,
    turn_warned: RwLock<bool>,
    on_exhausted: RwLock<OnExhausted>,
}

impl Default for CostTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl CostTracker {
    pub fn new() -> Self {
        Self {
            inner: std::sync::Arc::new(CostInner {
                session_micro: AtomicU64::new(0),
                turn_micro: AtomicU64::new(0),
                session_cap_micro: AtomicU64::new(0),
                turn_cap_micro: AtomicU64::new(0),
                soft_warn_milli: AtomicU64::new(500),
                session_warned: RwLock::new(false),
                turn_warned: RwLock::new(false),
                on_exhausted: RwLock::new(OnExhausted::Ask),
            }),
        }
    }

    /// Install caps and policy from config. Idempotent.
    pub fn install_config(&self, cfg: &kod_config::LimitsConfig) {
        self.inner
            .session_cap_micro
            .store(usd_to_micro(cfg.max_cost_usd_per_session), Ordering::Relaxed);
        self.inner
            .turn_cap_micro
            .store(usd_to_micro(cfg.max_cost_usd_per_turn), Ordering::Relaxed);
        self.inner
            .soft_warn_milli
            .store((cfg.soft_warn_at * 1000.0).max(0.0) as u64, Ordering::Relaxed);
        *self.inner.on_exhausted.write() = cfg.on_exhausted;
    }

    /// Record a completed provider call's cost.
    pub fn record(&self, cost_usd: f64) {
        if !cost_usd.is_finite() || cost_usd <= 0.0 {
            return;
        }
        let micro = usd_to_micro(cost_usd);
        self.inner.session_micro.fetch_add(micro, Ordering::Relaxed);
        self.inner.turn_micro.fetch_add(micro, Ordering::Relaxed);
    }

    /// Reset the per-turn counter. Called at the start of a new turn.
    pub fn begin_turn(&self) {
        self.inner.turn_micro.store(0, Ordering::Relaxed);
        *self.inner.turn_warned.write() = false;
    }

    /// Snapshot the state.
    /// Reset every counter. Called by `/budget reset`.
    pub fn reset(&self) {
        self.inner.session_micro.store(0, Ordering::Relaxed);
        self.inner.turn_micro.store(0, Ordering::Relaxed);
        *self.inner.session_warned.write() = false;
        *self.inner.turn_warned.write() = false;
    }

    pub fn snapshot(&self) -> CostSnapshot {
        let session_micro = self.inner.session_micro.load(Ordering::Relaxed);
        let turn_micro = self.inner.turn_micro.load(Ordering::Relaxed);
        let session_cap_micro = self.inner.session_cap_micro.load(Ordering::Relaxed);
        let turn_cap_micro = self.inner.turn_cap_micro.load(Ordering::Relaxed);
        let soft = self.inner.soft_warn_milli.load(Ordering::Relaxed) as f64 / 1000.0;

        let session_fraction = if session_cap_micro > 0 {
            (session_micro as f64 / session_cap_micro as f64).min(1.0)
        } else {
            0.0
        };
        let turn_fraction = if turn_cap_micro > 0 {
            (turn_micro as f64 / turn_cap_micro as f64).min(1.0)
        } else {
            0.0
        };
        let exhausted = (session_cap_micro > 0 && session_micro >= session_cap_micro)
            || (turn_cap_micro > 0 && turn_micro >= turn_cap_micro);
        CostSnapshot {
            session_usd: micro_to_usd(session_micro),
            session_cap_usd: micro_to_usd(session_cap_micro),
            turn_usd: micro_to_usd(turn_micro),
            turn_cap_usd: micro_to_usd(turn_cap_micro),
            session_fraction,
            turn_fraction,
            session_warned: soft > 0.0 && session_fraction >= soft,
            turn_warned: soft > 0.0 && turn_fraction >= soft,
            exhausted,
        }
    }

    /// Poll for a soft warning. Returns the trigger exactly once per
    /// crossing.
    pub fn poll_soft_warning(&self) -> Option<SoftWarningTrigger> {
        let snap = self.snapshot();
        if snap.session_warned {
            let mut w = self.inner.session_warned.write();
            if !*w {
                *w = true;
                return Some(SoftWarningTrigger::Session);
            }
        }
        if snap.turn_warned {
            let mut w = self.inner.turn_warned.write();
            if !*w {
                *w = true;
                return Some(SoftWarningTrigger::Turn);
            }
        }
        None
    }

    /// The configured exhaustion policy.
    pub fn on_exhausted(&self) -> OnExhausted {
        *self.inner.on_exhausted.read()
    }

    /// Raise the session cap by `delta_usd`. Used by `/raise N`.
    pub fn raise_session_cap(&self, delta_usd: f64) {
        if !delta_usd.is_finite() || delta_usd <= 0.0 {
            return;
        }
        let delta = usd_to_micro(delta_usd);
        self.inner.session_cap_micro.fetch_add(delta, Ordering::Relaxed);
        *self.inner.session_warned.write() = false;
    }
}

fn usd_to_micro(usd: f64) -> u64 {
    if !usd.is_finite() || usd <= 0.0 {
        return 0;
    }
    (usd * 1_000_000.0).round() as u64
}

fn micro_to_usd(micro: u64) -> f64 {
    micro as f64 / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use kod_config::limits::{LimitsConfig, OnExhausted};

    #[test]
    fn new_tracker_is_zero() {
        let t = CostTracker::new();
        let s = t.snapshot();
        assert_eq!(s.session_usd, 0.0);
        assert_eq!(s.turn_usd, 0.0);
        assert!(!s.exhausted);
    }

    #[test]
    fn record_accumulates_session_and_turn() {
        let t = CostTracker::new();
        t.record(0.01);
        t.record(0.02);
        let s = t.snapshot();
        assert!((s.session_usd - 0.03).abs() < 1e-6);
        assert!((s.turn_usd - 0.03).abs() < 1e-6);
    }

    #[test]
    fn record_ignores_zero_and_negative_and_nan() {
        let t = CostTracker::new();
        t.record(0.0);
        t.record(-1.0);
        t.record(f64::NAN);
        let s = t.snapshot();
        assert_eq!(s.session_usd, 0.0);
    }

    #[test]
    fn begin_turn_resets_only_turn() {
        let t = CostTracker::new();
        t.record(0.05);
        t.begin_turn();
        let s = t.snapshot();
        assert!((s.session_usd - 0.05).abs() < 1e-6);
        assert_eq!(s.turn_usd, 0.0);
    }

    #[test]
    fn session_cap_exhaustion_fires() {
        let t = CostTracker::new();
        let cfg = LimitsConfig {
            max_cost_usd_per_session: 1.0,
            ..Default::default()
        };
        t.install_config(&cfg);
        assert!(!t.snapshot().exhausted);
        t.record(1.5);
        assert!(t.snapshot().exhausted);
    }

    #[test]
    fn turn_cap_exhaustion_fires_independently_of_session() {
        let t = CostTracker::new();
        let cfg = LimitsConfig {
            max_cost_usd_per_session: 100.0,
            max_cost_usd_per_turn: 0.10,
            ..Default::default()
        };
        t.install_config(&cfg);
        t.record(0.15);
        assert!(t.snapshot().exhausted);
        t.begin_turn();
        assert!(!t.snapshot().exhausted);
        // Session cap still not exceeded.
        assert!(t.snapshot().session_usd < 100.0);
    }

    #[test]
    fn soft_warning_fires_once_per_crossing() {
        let t = CostTracker::new();
        let cfg = LimitsConfig {
            max_cost_usd_per_session: 1.0,
            soft_warn_at: 0.5,
            ..Default::default()
        };
        t.install_config(&cfg);
        t.record(0.4);
        assert!(t.poll_soft_warning().is_none());
        t.record(0.2);
        assert_eq!(t.poll_soft_warning(), Some(SoftWarningTrigger::Session));
        // Second poll within the same crossing returns None.
        assert!(t.poll_soft_warning().is_none());
    }

    #[test]
    fn raise_session_cap_adds_to_the_cap() {
        let t = CostTracker::new();
        let cfg = LimitsConfig {
            max_cost_usd_per_session: 1.0,
            ..Default::default()
        };
        t.install_config(&cfg);
        t.record(1.5);
        assert!(t.snapshot().exhausted);
        t.raise_session_cap(1.0);
        assert!(!t.snapshot().exhausted);
        assert!((t.snapshot().session_cap_usd - 2.0).abs() < 1e-6);
    }

    #[test]
    fn on_exhausted_round_trips() {
        let t = CostTracker::new();
        let mut cfg = LimitsConfig::default();
        cfg.on_exhausted = OnExhausted::Stop;
        t.install_config(&cfg);
        assert_eq!(t.on_exhausted(), OnExhausted::Stop);
    }

    #[test]
    fn exhausted_is_false_when_no_caps_are_set() {
        let t = CostTracker::new();
        t.record(1000.0);
        assert!(!t.snapshot().exhausted);
    }
}
