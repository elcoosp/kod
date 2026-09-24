//! Process-wide pause gate (borrow from oh-my-pi, delta §9.8).
//!
//! # The problem
//!
//! A user needs to stop the agent from doing anything — a meeting, a
//! shift of attention, a "wait, I need to check something first" — and
//! then resume it. Cancelling a run destroys work in flight; pausing
//! should not. But a naive "check for pause" at every await point is
//! both invasive (every loop, every helper) and wrong: a stream that
//! is mid-flight must run to completion, not be torn down at the
//! check.
//!
//! # The shape
//!
//! [`PauseGate`] is a synchronization primitive, not a state machine.
//! It has exactly two operations — `pause()` and `resume()` — and one
//! query — `wait_if_paused()`, an async method that returns
//! immediately when the gate is running and parks the caller when it
//! is paused.
//!
//! Callers poll the gate **at exactly two boundaries**, and only at
//! those boundaries: before each model call and before each tool
//! call. That is what makes "in-flight work runs to completion"
//! automatic — the gate cannot interrupt a stream because nothing
//! polls it mid-stream. The design note's "polled by every loop at
//! exactly two boundaries" is the *caller's* contract; the gate
//! offers the primitive and stays out of the way.
//!
//! # Abort is orthogonal
//!
//! The design note says "abort parks, never releases the gate". That
//! is a rule for the caller's **abort handler**, not a rule for the
//! gate. The gate has no notion of abort; the abort handler's job is
//! to *not call `resume()`*. Keeping that rule at exactly one call
//! site (the abort handler) is deliberate: a gate that understood
//! abort would have two ways to decide "are we running?", and those
//! two would eventually disagree.
//!
//! Concretely: a user pauses, then aborts the current in-flight run.
//! The gate stays paused. The abort released *the run*, not the
//! gate. The user calls `resume()` when they want work to continue,
//! and the next run starts under the same paused-or-running state the
//! gate already carries.
//!
//! # Re-engage safe
//!
//! `resume()` on an already-running gate is a no-op; `pause()` on an
//! already-paused one is a no-op. A user pressing the pause key twice
//! does not need the caller to dedupe. `resume()` after a `resume()`
//! wakes any waiters that arrived between the two calls — the wait
//! loop re-checks the state after waking, so a waiter that missed the
//! first wake by nanoseconds does not park forever.
//!
//! # What this is NOT
//!
//! * Not a cancellation token. Cancellation unwinds a run; the pause
//!   gate parks one. A cancelled run's `CancellationToken` is a
//!   different primitive (`tokio_util::sync::CancellationToken`), and
//!   the two compose: a run can be cancelled while the gate is paused,
//!   and the gate is still paused afterwards.
//!
//! * Not a queue. There is no "what was queued during the pause?"
//!   here. A steer that arrives while paused is the engine's concern
//!   — it holds it in its own queue and delivers it after `resume()`.
//!   The gate's only job is to hold the caller at a boundary.

use tokio::sync::watch;

/// A process-wide pause gate.
///
/// Cheap to clone via `Arc`; the engine holds one and hands clones to
/// every loop. The state is a single boolean behind a `watch`
/// channel, so waiters see every transition and re-check the state
/// after each wake.
pub struct PauseGate {
    /// `true` means paused. Sending to the channel notifies every
    /// waiter; the value they read on wake tells them whether to
    /// return or park again.
    tx: watch::Sender<bool>,
    /// Retained so `wait_if_paused` can subscribe. Holding this in
    /// the struct also keeps the channel alive even when every
    /// external receiver has been dropped — the `tx.send` call never
    /// fails.
    rx: watch::Receiver<bool>,
}

impl PauseGate {
    /// A fresh gate, running.
    pub fn new() -> Self {
        let (tx, rx) = watch::channel(false);
        Self { tx, rx }
    }

    /// Pause. Idempotent: pausing an already-paused gate is a no-op
    /// (though it still emits a notification, which is harmless
    /// because waiters re-check state on wake).
    pub fn pause(&self) {
        // `send` only fails if every receiver has been dropped; the
        // struct holds `rx`, so this never fails in practice. The
        // `let _` is defensive.
        let _ = self.tx.send(true);
    }

    /// Resume. Idempotent for the same reason.
    pub fn resume(&self) {
        let _ = self.tx.send(false);
    }

    /// Is the gate currently paused?
    ///
    /// Cheap (an atomic load); safe to call from a UI render loop to
    /// pick a label. Not a substitute for `wait_if_paused` at a
    /// boundary — a caller that checks this and then proceeds has a
    /// race between the check and the proceed.
    pub fn is_paused(&self) -> bool {
        *self.rx.borrow()
    }

    /// The gate's one async method. Returns immediately when running;
    /// parks the caller when paused and returns the next time the
    /// gate is running.
    ///
    /// Call this *only* at a boundary. Between two await points
    /// inside a stream, calling it would mean the gate can interrupt
    /// work in flight — which is exactly what it must not do.
    pub async fn wait_if_paused(&self) {
        // `wait_for` subscribes to the watch channel and returns
        // immediately if the predicate is already satisfied, so this
        // is a single method call with no manual retry loop.
        let mut rx = self.rx.clone();
        // A dropped sender would resolve the wait with an error; the
        // gate never drops its sender while `self` is alive, so this
        // cannot happen in practice. The `let _` matches `pause` and
        // `resume`.
        let _ = rx.wait_for(|&paused| !paused).await;
    }
}

impl Default for PauseGate {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for PauseGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The channel's inner `bool` is not `Debug` on `watch::Sender`,
        // so spell it out.
        f.debug_struct("PauseGate")
            .field("is_paused", &self.is_paused())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn a_fresh_gate_is_running() {
        let g = PauseGate::new();
        assert!(!g.is_paused());
    }

    #[test]
    fn pause_makes_it_paused() {
        let g = PauseGate::new();
        g.pause();
        assert!(g.is_paused());
    }

    #[test]
    fn resume_makes_it_running_again() {
        let g = PauseGate::new();
        g.pause();
        g.resume();
        assert!(!g.is_paused());
    }

    #[test]
    fn pause_is_idempotent() {
        let g = PauseGate::new();
        g.pause();
        g.pause();
        assert!(g.is_paused());
    }

    #[test]
    fn resume_is_idempotent() {
        let g = PauseGate::new();
        g.resume();
        g.resume();
        assert!(!g.is_paused());
    }

    #[tokio::test]
    async fn wait_if_paused_returns_immediately_when_running() {
        let g = PauseGate::new();
        // The test would hang if this did not return; use a timeout
        // to fail loudly rather than deadlock the whole test runner.
        tokio::time::timeout(Duration::from_millis(100), g.wait_if_paused())
            .await
            .expect("wait_if_paused on a running gate must return immediately");
    }

    #[tokio::test]
    async fn wait_if_paused_parks_when_paused() {
        let g = Arc::new(PauseGate::new());
        g.pause();
        // The wait must NOT return within a short window — if it did,
        // the gate would be telling the caller to proceed while the
        // user asked it to stop.
        let wait = tokio::spawn({
            let g = g.clone();
            async move { g.wait_if_paused().await }
        });
        let early = tokio::time::timeout(Duration::from_millis(50), wait).await;
        assert!(
            early.is_err(),
            "wait_if_paused must park while paused, returned early",
        );
    }

    #[tokio::test]
    async fn resume_wakes_a_parked_waiter() {
        let g = Arc::new(PauseGate::new());
        g.pause();

        let handle = tokio::spawn({
            let g = g.clone();
            async move { g.wait_if_paused().await }
        });

        // Give the waiter a moment to actually park; then resume.
        tokio::time::sleep(Duration::from_millis(20)).await;
        g.resume();

        tokio::time::timeout(Duration::from_millis(200), handle)
            .await
            .expect("resume must wake the parked waiter")
            .expect("waiter task must not panic");
    }

    #[tokio::test]
    async fn multiple_waiters_all_wake_on_resume() {
        let g = Arc::new(PauseGate::new());
        g.pause();

        let mut handles = Vec::new();
        for _ in 0..8 {
            let g = g.clone();
            handles.push(tokio::spawn(async move { g.wait_if_paused().await }));
        }

        tokio::time::sleep(Duration::from_millis(20)).await;
        g.resume();

        for h in handles {
            tokio::time::timeout(Duration::from_millis(200), h)
                .await
                .expect("every waiter must wake on resume")
                .expect("no waiter panics");
        }
    }

    #[tokio::test]
    async fn re_pausing_after_resume_parks_again() {
        // The gate is re-engageable: resume does not "use up" the
        // pause; a subsequent pause parks a fresh waiter.
        let g = Arc::new(PauseGate::new());
        g.pause();
        g.resume();

        // A wait on a running gate returns immediately.
        tokio::time::timeout(Duration::from_millis(100), g.wait_if_paused())
            .await
            .expect("running gate admits waiters");

        // Pausing again parks a fresh wait.
        g.pause();
        let handle = tokio::spawn({
            let g = g.clone();
            async move { g.wait_if_paused().await }
        });
        let early = tokio::time::timeout(Duration::from_millis(50), handle).await;
        assert!(
            early.is_err(),
            "re-paused gate must park a fresh waiter",
        );
    }

    #[tokio::test]
    async fn a_waiter_that_arrives_just_after_resume_does_not_park() {
        // The classic "missed notification" race: resume fires, then
        // a waiter arrives and checks state. With `watch`, the
        // current value is `false` (running), so the waiter returns
        // immediately. A `Notify`-based implementation would be
        // fragile here; the test pins the property.
        let g = Arc::new(PauseGate::new());
        g.pause();
        g.resume();
        // Waiter arrives AFTER the resume. Must not park.
        tokio::time::timeout(Duration::from_millis(100), g.wait_if_paused())
            .await
            .expect("waiter arriving after resume must not park");
    }

    #[tokio::test]
    async fn abort_is_orthogonal_to_the_gate() {
        // The design note's "abort parks, never releases the gate"
        // is a *caller-side* rule: the abort handler must not call
        // resume. The gate's only job is to stay in whatever state
        // it was put in. This test simulates a caller that correctly
        // follows the rule — pause, abort (no resume), verify
        // still paused — and then resumes explicitly.
        let g = PauseGate::new();
        g.pause();
        assert!(g.is_paused());

        // A caller's abort handler would run here. Per the doc, it
        // does NOT call `g.resume()`. The gate stays paused.
        // (Simulating the abort as a no-op on the gate.)
        assert!(
            g.is_paused(),
            "an abort that follows the rule must leave the gate paused",
        );

        // Explicit resume clears the pause.
        g.resume();
        assert!(!g.is_paused());
    }

    #[tokio::test]
    async fn concurrent_pause_resume_does_not_deadlock() {
        // Rapid pause/resume from one task while another waits. The
        // test is about liveness: a naive implementation that
        // queued notifications could wedge a waiter forever under
        // this pattern. Runs 200 iterations in a tight loop; if the
        // waiter ever stalls, the timeout fires.
        let g = Arc::new(PauseGate::new());
        g.pause();

        let toggler = tokio::spawn({
            let g = g.clone();
            async move {
                for _ in 0..200 {
                    g.resume();
                    g.pause();
                    tokio::task::yield_now().await;
                }
                // End running so the waiter can exit.
                g.resume();
            }
        });

        let waiter = tokio::spawn({
            let g = g.clone();
            async move {
                // In a loop, wait whenever paused. Each call returns
                // when running; loop again for the next pause.
                for _ in 0..200 {
                    g.wait_if_paused().await;
                    tokio::task::yield_now().await;
                }
            }
        });

        tokio::time::timeout(Duration::from_secs(5), async {
            let _ = toggler.await;
            let _ = waiter.await;
        })
        .await
        .expect("concurrent pause/resume must not deadlock");
    }

    #[test]
    fn debug_impl_reports_the_state() {
        // A caller logging the gate should see its state, not an
        // opaque handle.
        let g = PauseGate::new();
        let d = format!("{g:?}");
        assert!(d.contains("is_paused: false"), "got: {d}");
        g.pause();
        let d = format!("{g:?}");
        assert!(d.contains("is_paused: true"), "got: {d}");
    }

    #[test]
    fn default_matches_new() {
        // `Default` is documented as `new`, and a user that calls one
        // or the other must get a running gate.
        let a = PauseGate::default();
        let b = PauseGate::new();
        assert_eq!(a.is_paused(), b.is_paused());
    }
}
