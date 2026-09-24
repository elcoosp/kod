//! Per-provider concurrency bracket (borrow from oh-my-pi, delta §9.9).
//!
//! # The failure this prevents
//!
//! A naive concurrency cap wraps the *whole agent* — acquire on spawn,
//! release on completion. That is a deadlock waiting to happen the
//! moment a spawned child shares a slot: the parent holds its own
//! slot, waits on the child, the child waits on a slot the parent is
//! holding, nobody moves. The workspace's issue log carries this as
//! #3749.
//!
//! The doc's rule is the fix: the cap is wrapped around **only the
//! streaming HTTP request** — the network I/O — and released the
//! moment the response finishes producing. The agent's own lifetime
//! is unrelated to slot occupancy. A parent can hold a slot while its
//! children hold their own; slots are I/O-shaped, not task-shaped.
//!
//! # What this module provides
//!
//! [`ProviderConcurrency`] is the primitive: a per-endpoint
//! admission counter with an async [`acquire`] that parks when the
//! cap is reached and releases on permit drop. The caller's
//! obligation — wrap it around the request, not the agent — is
//! documented here and enforced by convention; the primitive cannot
//! observe it.
//!
//! # Semantics
//!
//! * **`cap == 0` means unbounded.** Every acquire succeeds
//!   immediately. This is the default for a provider whose endpoint
//!   config carries no limit; it makes "no configuration" and "no
//!   cap" the same thing, which is what a user expects.
//!
//! * **Resize-down overshoots, never kills.** When `set_cap` shrinks
//!   below the current in-flight count, the running requests are
//!   *not* cancelled — they finish normally. New acquires are simply
//!   refused until `in_flight < cap` again. Forcible interruption is
//!   a cancellation-token concern and belongs to a different
//!   primitive (the engine holds one).
//!
//! * **Resize-up wakes waiters.** `set_cap` to a larger value calls
//!   `notify_waiters`, so every parked acquirer re-checks state.
//!   Waiters whose check still fails (because someone else got
//!   there first) re-park; the loop is deadlock-free because each
//!   release generates one notification.
//!
//! * **Cancellable acquire, by construction.** `acquire` returns a
//!   future; wrapping it in `tokio::select!` and losing the race
//!   drops the future without ever having incremented `in_flight`.
//!   The counter cannot leak on a cancelled acquire.
//!
//! # What this module is NOT
//!
//! * Not a rate limiter. A rate limiter throttles *attempts per unit
//!   time*; this is a cap on *concurrent in-flight requests*.
//!   Different problem, different tool.
//!
//! * Not a general-purpose semaphore. It deliberately exposes only
//!   what the endpoint-concurrency use case needs — acquire, release
//!   (via drop), read state, resize — and nothing else. A caller
//!   that wants a `try_acquire`-heavy pattern has it; a caller that
//!   wants to hand permits around the process does not, and should
//!   not.

use std::sync::Arc;
// `std::sync::Mutex`, not `parking_lot::Mutex`: the lock is held
// only for the duration of a few integer reads/writes and never
// across an `.await`. `parking_lot`'s advantages (speed under
// contention, no poisoning) do not justify a new dependency in a
// crate whose dependency list is otherwise tight. The poisoning
// cascade — every `lock()` after a panic returns `Err` — is
// neutralized below by recovering the guard from the poisoned
// state; the counter's integer cannot be corrupted by a panic.
use std::sync::Mutex;
use tokio::sync::Notify;

/// Lock the state mutex, recovering from a poisoned lock.
///
/// A poisoned lock means another thread panicked while holding it.
/// The state here is two `usize`s; a panic elsewhere in the process
/// cannot have left those integers in an invalid state (no
/// invariant spans two fields in a way that a panic between writes
/// could break). Recovering the guard and proceeding is preferable
/// to propagating the poison as a cascade of panics that would take
/// the whole provider stack down over a problem that does not exist.
fn lock_recover(
    m: &Mutex<State>,
) -> std::sync::MutexGuard<'_, State> {
    match m.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// A per-provider admission counter.
///
/// Clone via `Arc` when sharing across tasks. Every task that wants
/// to issue a request acquires a [`ConcurrencyPermit`]; the permit
/// releases on drop. Holding a permit across an `await` is
/// intentional (that is the whole point); holding one *forever* is
/// what the doc's "wrap around the request, not the agent" rule
/// forbids.
pub struct ProviderConcurrency {
    inner: Arc<Inner>,
}

struct Inner {
    state: Mutex<State>,
    notify: Notify,
}

#[derive(Debug)]
struct State {
    /// `0` means unbounded. A non-zero value is a hard cap on
    /// concurrent permits.
    cap: usize,
    /// Permits currently held. May exceed `cap` temporarily after a
    /// resize-down; the doc's "overshoot" case.
    in_flight: usize,
}

impl State {
    fn can_acquire(&self) -> bool {
        self.cap == 0 || self.in_flight < self.cap
    }
}

/// A held slot. Releases on drop.
///
/// Not `Clone`: a permit corresponds to one held slot. A caller that
/// wants to hand the slot to another task transfers the permit.
pub struct ConcurrencyPermit {
    /// Keeps the shared state alive even if the owning
    /// `ProviderConcurrency` is dropped while permits are still
    /// outstanding. That ordering is legal — a caller can drop the
    /// registry entry while a request is in flight — and the permit
    /// must still release cleanly.
    inner: Arc<Inner>,
}

impl ProviderConcurrency {
    /// A fresh counter with `cap` as its initial limit. `0` = no cap.
    pub fn new(cap: usize) -> Self {
        Self {
            inner: Arc::new(Inner {
                state: Mutex::new(State { cap, in_flight: 0 }),
                notify: Notify::new(),
            }),
        }
    }

    /// The current cap. `0` means unbounded.
    pub fn cap(&self) -> usize {
        lock_recover(&self.inner.state).cap
    }

    /// The current number of held permits. May exceed [`Self::cap`]
    /// after a resize-down, until the overshoot drains.
    pub fn in_flight(&self) -> usize {
        lock_recover(&self.inner.state).in_flight
    }

    /// Change the cap.
    ///
    /// Growing wakes every waiter. Shrinking silently refuses new
    /// acquires until the overshoot drains; in-flight permits are
    /// not revoked.
    ///
    /// The "grew" test compares permission, not magnitude: a move
    /// from `cap = 5` to `cap = 0` is a growth (unbounded is more
    /// permissive than any finite cap), and a move from `cap = 0` to
    /// anything is a shrink. Getting this backwards would either
    /// strand waiters after an unbounded upgrade, or spurious-wake
    /// them on a shrink.
    pub fn set_cap(&self, cap: usize) {
        let grew = {
            let mut s = lock_recover(&self.inner.state);
            let old = s.cap;
            s.cap = cap;
            match (old, cap) {
                // Already unbounded, still unbounded: no change.
                (0, 0) => false,
                // Becoming unbounded is always a growth.
                (_, 0) => true,
                // Leaving unbounded is a shrink.
                (0, _) => false,
                // Two finite caps: growth iff strictly larger.
                (a, b) => b > a,
            }
        };
        if grew {
            // Wake every waiter; each re-checks state. A waiter that
            // still cannot acquire (because another got there first)
            // re-parks. This is what makes resize-up correct even
            // with a horde of parked callers.
            self.inner.notify.notify_waiters();
        }
    }

    /// Try to acquire without waiting. `None` when at cap.
    ///
    /// A caller that wants "use a slot if available, else do
    /// something else" uses this. A caller that wants "wait for a
    /// slot" uses [`Self::acquire`].
    pub fn try_acquire(&self) -> Option<ConcurrencyPermit> {
        let mut s = lock_recover(&self.inner.state);
        if s.can_acquire() {
            s.in_flight += 1;
            drop(s);
            Some(ConcurrencyPermit {
                inner: Arc::clone(&self.inner),
            })
        } else {
            None
        }
    }

    /// Acquire a slot, parking while the cap is reached.
    ///
    /// The returned future is cancellable by dropping: nothing is
    /// incremented until just before the future resolves, so
    /// `select!`-ing this against a cancellation token leaves no
    /// leaked slot when the token wins.
    pub async fn acquire(&self) -> ConcurrencyPermit {
        loop {
            if let Some(p) = self.try_acquire() {
                return p;
            }
            // Register as a waiter. `Notify::notified()` returns a
            // future; `.await` on it blocks until a wake. The
            // classic `Notify` race — "notification fires between the
            // failed try_acquire and the `.await`" — is closed by
            // `notify_one` storing a permit when no waiter is
            // registered: the next `notified().await` consumes that
            // permit immediately. A `notify_waiters` on release would
            // reopen the race; the release path uses `notify_one`
            // precisely to avoid it.
            self.inner.notify.notified().await;
        }
    }
}

impl Default for ProviderConcurrency {
    fn default() -> Self {
        // The doc's "≤0 = unbounded" default applied to the absence
        // of a configuration: a fresh instance is uncapped, which is
        // what a provider without a `[concurrency]` block should do.
        Self::new(0)
    }
}

impl Drop for ConcurrencyPermit {
    fn drop(&mut self) {
        {
            let mut s = lock_recover(&self.inner.state);
            // `saturating_sub` because dropping an extra permit — a
            // caller double-releasing, or a permit outliving a
            // pathological sequence — must not wrap the counter.
            // The counter is a hint; the invariant is that
            // `in_flight >= 0`, not that it always matches the
            // number of live permits.
            s.in_flight = s.in_flight.saturating_sub(1);
        }
        // Wake exactly one waiter. Every release corresponds to one
        // freed slot, so one wake is the right cadence; a horde of
        // waiters is handled by the loop, not by a broadcast.
        self.inner.notify.notify_one();
    }
}

impl std::fmt::Debug for ProviderConcurrency {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = lock_recover(&self.inner.state);
        f.debug_struct("ProviderConcurrency")
            .field("cap", &s.cap)
            .field("in_flight", &s.in_flight)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::timeout;

    #[test]
    fn a_fresh_counter_starts_at_cap_with_nothing_in_flight() {
        let c = ProviderConcurrency::new(4);
        assert_eq!(c.cap(), 4);
        assert_eq!(c.in_flight(), 0);
    }

    #[test]
    fn default_is_unbounded() {
        // A provider with no configuration accepts everything. The
        // `Default` impl is the "no config" path.
        let c = ProviderConcurrency::default();
        assert_eq!(c.cap(), 0);
    }

    #[test]
    fn a_permit_counts_toward_in_flight() {
        let c = ProviderConcurrency::new(3);
        let _p = c.try_acquire().unwrap();
        assert_eq!(c.in_flight(), 1);
    }

    #[test]
    fn dropping_a_permit_releases_it() {
        let c = ProviderConcurrency::new(3);
        {
            let _p = c.try_acquire().unwrap();
            assert_eq!(c.in_flight(), 1);
        }
        assert_eq!(c.in_flight(), 0);
    }

    #[test]
    fn try_acquire_refuses_at_cap() {
        let c = ProviderConcurrency::new(2);
        let _a = c.try_acquire().unwrap();
        let _b = c.try_acquire().unwrap();
        assert!(c.try_acquire().is_none());
    }

    #[test]
    fn cap_zero_is_unbounded() {
        let c = ProviderConcurrency::new(0);
        let mut permits = Vec::new();
        for _ in 0..1_000 {
            permits.push(c.try_acquire().unwrap());
        }
        assert_eq!(c.in_flight(), 1_000);
    }

    #[tokio::test]
    async fn acquire_returns_immediately_when_under_cap() {
        let c = ProviderConcurrency::new(4);
        timeout(Duration::from_millis(100), c.acquire())
            .await
            .expect("acquire under cap must not park");
    }

    #[tokio::test]
    async fn acquire_parks_when_at_cap() {
        let c = Arc::new(ProviderConcurrency::new(1));
        let _p = c.try_acquire().unwrap();
        // A second acquire must not complete while the first is held.
        let waiter = tokio::spawn({
            let c = Arc::clone(&c);
            async move {
                let _p = c.acquire().await;
                // Hold for a beat so the test can verify parking.
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });
        let early = timeout(Duration::from_millis(60), waiter).await;
        assert!(
            early.is_err(),
            "acquire must park while cap is reached",
        );
    }

    #[tokio::test]
    async fn dropping_a_permit_wakes_a_parked_acquirer() {
        let c = Arc::new(ProviderConcurrency::new(1));
        let p = c.try_acquire().unwrap();

        let acquired = Arc::new(tokio::sync::Notify::new());
        let acquired_clone = Arc::clone(&acquired);
        let c_clone = Arc::clone(&c);
        let waiter = tokio::spawn(async move {
            let _p = c_clone.acquire().await;
            acquired_clone.notify_one();
        });

        // Give the waiter a moment to park, then release the slot.
        tokio::time::sleep(Duration::from_millis(20)).await;
        drop(p);

        timeout(Duration::from_millis(500), acquired.notified())
            .await
            .expect("dropping a permit must wake the waiter");

        let _ = waiter.await;
    }

    #[tokio::test]
    async fn many_waiters_are_served_serially() {
        // Six slots requested against a cap of 2. Every acquire
        // eventually succeeds; the counter never exceeds the cap.
        let c = Arc::new(ProviderConcurrency::new(2));
        let mut handles = Vec::new();
        for _ in 0..6 {
            let c = Arc::clone(&c);
            handles.push(tokio::spawn(async move {
                let p = c.acquire().await;
                // Hold briefly so concurrency is observable.
                tokio::time::sleep(Duration::from_millis(10)).await;
                drop(p);
            }));
        }

        // The whole batch must complete within a generous bound. The
        // bound scales with (requests / cap) * hold-time — 3 * 10 ms
        // plus scheduling slack.
        timeout(Duration::from_secs(2), async {
            for h in handles {
                let _ = h.await;
            }
        })
        .await
        .expect("all waiters must be served, none stranded");

        assert_eq!(c.in_flight(), 0, "every permit must have been released");
    }

    #[tokio::test]
    async fn resize_up_wakes_waiters() {
        let c = Arc::new(ProviderConcurrency::new(1));
        let _p = c.try_acquire().unwrap();

        let waiter = tokio::spawn({
            let c = Arc::clone(&c);
            async move {
                let _p = c.acquire().await;
            }
        });

        // Park the waiter.
        tokio::time::sleep(Duration::from_millis(20)).await;
        // Grow the cap. This must wake the waiter even though no
        // permit was released.
        c.set_cap(2);
        timeout(Duration::from_millis(500), waiter)
            .await
            .expect("resize-up must wake parked acquirers")
            .expect("waiter task must not panic");
    }

    #[test]
    fn resize_down_refuses_new_acquires_until_overshoot_drains() {
        let c = ProviderConcurrency::new(4);
        let p1 = c.try_acquire().unwrap();
        let p2 = c.try_acquire().unwrap();
        let p3 = c.try_acquire().unwrap();
        assert_eq!(c.in_flight(), 3);

        // Shrink below the current in-flight count.
        c.set_cap(1);
        assert_eq!(c.cap(), 1);
        // Overshoot persists: the in-flight requests were not killed.
        assert_eq!(c.in_flight(), 3);

        // New acquires are refused while the overshoot is in place.
        assert!(c.try_acquire().is_none());

        // Release two; still at cap (in_flight == cap == 1).
        drop(p1);
        drop(p2);
        assert_eq!(c.in_flight(), 1);
        assert!(c.try_acquire().is_none());

        // Release the third; now under cap.
        drop(p3);
        assert_eq!(c.in_flight(), 0);
        assert!(c.try_acquire().is_some());
    }

    #[test]
    fn resize_from_finite_to_zero_wakes_and_admits_everything() {
        // Moving to `cap = 0` is a growth, not a shrink: any finite
        // cap is less permissive than unbounded.
        let c = ProviderConcurrency::new(1);
        let _p = c.try_acquire().unwrap();
        c.set_cap(0);
        assert_eq!(c.cap(), 0);
        // Unbounded admits a second acquire even with one held.
        assert!(c.try_acquire().is_some());
    }

    #[test]
    fn resize_from_zero_to_finite_is_a_shrink() {
        // Moving away from unbounded is a shrink. A caller that had
        // ten requests in flight under `cap = 0` and then sets
        // `cap = 2` must not see new acquires succeed until the
        // in-flight count drops below 2.
        let c = ProviderConcurrency::new(0);
        let _a = c.try_acquire().unwrap();
        let _b = c.try_acquire().unwrap();
        let _c = c.try_acquire().unwrap();
        c.set_cap(2);
        assert_eq!(c.cap(), 2);
        assert_eq!(c.in_flight(), 3);
        assert!(c.try_acquire().is_none());
    }

    #[tokio::test]
    async fn a_cancelled_acquire_does_not_leak_in_flight() {
        // The doc's "abortable acquire" property, pinned. A caller
        // does `select! { _ = c.acquire() => .., _ = token.cancelled()
        // => .. }`. If the token wins, the acquire future is dropped
        // mid-wait; `in_flight` must not have incremented.
        let c = Arc::new(ProviderConcurrency::new(1));
        let _p = c.try_acquire().unwrap();
        assert_eq!(c.in_flight(), 1);

        // Spawn the acquire, drop it mid-wait by using timeout.
        let fut = c.acquire();
        let result = timeout(Duration::from_millis(30), fut).await;
        assert!(result.is_err(), "acquire should still be parked");
        // The timeout dropped the future. The counter must be
        // unchanged: nothing was incremented for a parked acquire.
        assert_eq!(
            c.in_flight(),
            1,
            "a cancelled acquire must not have incremented in_flight",
        );
    }

    #[test]
    fn dropping_a_permit_more_than_once_does_not_underflow() {
        // Defensive against a caller that double-releases (impossible
        // through safe usage, but the counter must never wrap). We
        // can't call drop twice on the same permit through the type
        // system, so this test verifies the saturating_sub by
        // constructing a scenario where the counter is already at
        // zero when a permit would have decremented.
        let c = ProviderConcurrency::new(1);
        let p = c.try_acquire().unwrap();
        // Manually decrement out from under the permit to simulate
        // corruption.
        {
            let mut s = lock_recover(&c.inner.state);
            s.in_flight = 0;
        }
        // Now dropping the permit must saturate, not wrap.
        drop(p);
        assert_eq!(c.in_flight(), 0);
    }

    #[tokio::test]
    async fn a_permit_outlives_its_provider() {
        // A caller drops the `ProviderConcurrency` (a registry
        // rebuild, a config reload) while a permit is still held.
        // The permit's `Arc<Inner>` keeps the state alive; dropping
        // the permit releases cleanly into state that no one else
        // can observe, but nothing panics or wraps.
        let permit = {
            let c = ProviderConcurrency::new(2);
            c.try_acquire().unwrap()
            // `c` drops here; the permit's `Arc<Inner>` keeps the
            // state alive.
        };
        // Nothing observable to assert, but if the drop panicked
        // the test would fail.
        drop(permit);
    }

    #[tokio::test]
    async fn concurrent_acquire_and_release_does_not_deadlock() {
        // Many tasks acquiring and releasing under a small cap. If
        // the wait loop had a lost-wakeup bug, this would stall.
        let c = Arc::new(ProviderConcurrency::new(3));
        let mut handles = Vec::new();
        for _ in 0..20 {
            let c = Arc::clone(&c);
            handles.push(tokio::spawn(async move {
                for _ in 0..10 {
                    let _p = c.acquire().await;
                    tokio::task::yield_now().await;
                }
            }));
        }

        timeout(Duration::from_secs(5), async {
            for h in handles {
                let _ = h.await;
            }
        })
        .await
        .expect("concurrent acquire/release must not deadlock");

        assert_eq!(c.in_flight(), 0);
    }

    #[test]
    fn debug_reports_cap_and_in_flight() {
        let c = ProviderConcurrency::new(7);
        let _p = c.try_acquire().unwrap();
        let d = format!("{c:?}");
        assert!(d.contains("cap: 7"), "got: {d}");
        assert!(d.contains("in_flight: 1"), "got: {d}");
    }
}
