//! YieldQueue: the typed aside pipeline (borrow from oh-my-pi,
//! delta §11.5).
//!
//! # The problem
//!
//! Background events reach a running turn from several sources: a
//! background job finishing, late LSP diagnostics arriving after a
//! write, an advisor raising a concern. Each source wants to inject
//! a message at a safe point — mid-stream when the model is
//! generating, or as a follow-up turn when it is idle.
//!
//! The pre-§11.5 shape was ad-hoc: each source had its own queue
//! and its own injection point, and none of them could express
//! "this is stale now, drop it" — a diagnosis that arrived three
//! turns after the code changed went into the transcript anyway.
//!
//! # The shape
//!
//! A **YieldQueue** is a per-kind queue of *thunks*. A source
//! registers a producer: a closure that either renders the event
//! into an injection string, or returns `None` when the event has
//! gone stale by the time the loop drains it. The staleness check
//! runs *at drain time*, not at queue time — which is what makes a
//! late diagnosis drop itself when the code it describes has
//! changed.
//!
//! Two flush modes:
//!
//! * **Streaming** — drained mid-turn, at the next round boundary.
//!   The loop calls `drain_streaming` and injects whatever it
//!   returns.
//! * **Idle** — batched into a follow-up turn when the model has
//!   nothing else to do. The loop calls `drain_idle` when it is
//!   about to stop and the queue is non-empty.
//!
//! A producer registered with `skip_idle_flush` is drained only by
//! `drain_streaming` — useful for a `Steer`-severity advisor note
//! that must reach the model mid-work rather than after.
//!
//! # What this does NOT do
//!
//! * Not the injection. The queue produces strings; the loop
//!   decides how to put them on the wire (a steer, a system
//!   message, a UI card).
//! * Not persistence. The queue is in-memory; an event that
//!   arrives and is never drained is lost. Callers that need
//!   durability write to a store first and register the queue
//!   entry as a delivery notification.

use std::collections::VecDeque;
use std::sync::Mutex;

/// A queued yield: the *kind* it was registered under, and the
/// thunk that renders it (or returns `None` when stale).
///
/// The kind is a short string (`"diagnostics"`, `"job-result"`,
/// `"advise"`). It is carried so a caller can attribute a drain
/// entry to its source without inspecting the rendered text.
pub struct QueuedYield {
    pub kind: String,
    /// Whether `drain_idle` should include this entry. A
    /// `skip_idle_flush` producer is drained only by
    /// `drain_streaming`.
    pub skip_idle_flush: bool,
    /// The thunk. Evaluated at drain time — the staleness check
    /// runs here, so it sees the state at drain, not at register.
    render: Box<dyn FnOnce() -> Option<String> + Send>,
}

impl QueuedYield {
    /// Evaluate the thunk. `None` means the item is stale.
    pub fn take_render(self) -> Option<String> {
        (self.render)()
    }
}

impl std::fmt::Debug for QueuedYield {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QueuedYield")
            .field("kind", &self.kind)
            .field("skip_idle_flush", &self.skip_idle_flush)
            .finish()
    }
}

/// One drain batch: the strings the caller should inject, plus a
/// count of entries that evaluated to `None` (stale, dropped).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct DrainBatch {
    /// Rendered messages, in registration order.
    pub messages: Vec<String>,
    /// How many entries were dropped as stale. A caller that wants
    /// to log "N drops" reads this.
    pub dropped: usize,
}

impl DrainBatch {
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }
}

/// The flush mode a drain targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlushMode {
    /// Drained mid-turn.
    Streaming,
    /// Drained when the loop is about to stop.
    Idle,
}

/// The queue.
///
/// Cheap to share: the inner `Mutex<VecDeque>` guards a short
/// critical section (a push or a drain). The thunks themselves
/// never run under the lock.
pub struct YieldQueue {
    inner: Mutex<VecDeque<QueuedYield>>,
    /// Whether idle flush is enabled for this queue. The loop can
    /// disable it while it is streaming, re-enable it at turn end.
    idle_flush_enabled: Mutex<bool>,
}

impl YieldQueue {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(VecDeque::new()),
            idle_flush_enabled: Mutex::new(true),
        }
    }

    /// Register a yield. `render` is called at drain time; a `None`
    /// return drops the entry.
    ///
    /// `skip_idle_flush` — whether `drain_idle` should pass over
    /// this entry. A `Steer`-severity advisor note sets it; a
    /// `Card`-severity note leaves it false.
    pub fn register<F>(&self, kind: impl Into<String>, skip_idle_flush: bool, render: F)
    where
        F: FnOnce() -> Option<String> + Send + 'static,
    {
        let entry = QueuedYield {
            kind: kind.into(),
            skip_idle_flush,
            render: Box::new(render),
        };
        if let Ok(mut q) = self.inner.lock() {
            q.push_back(entry);
        }
    }

    /// How many entries are queued (stale or not — staleness is
    /// only known at drain).
    pub fn len(&self) -> usize {
        self.inner.lock().map(|q| q.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Drain for a specific flush mode.
    ///
    /// Each entry's thunk runs. A `None` return is a drop; a `Some`
    /// is a message. Entries skipped by the mode (a
    /// `skip_idle_flush` entry on an idle drain) stay in the queue
    /// for a later streaming drain.
    pub fn drain(&self, mode: FlushMode) -> DrainBatch {
        // Pull the whole queue under the lock, then evaluate the
        // thunks outside it — a slow render must not block a
        // concurrent push.
        let mut queued: VecDeque<QueuedYield> = {
            match self.inner.lock() {
                Ok(mut q) => std::mem::take(&mut *q),
                Err(_) => return DrainBatch::default(),
            }
        };

        let mut batch = DrainBatch::default();
        let mut keep_back: VecDeque<QueuedYield> = VecDeque::new();

        while let Some(entry) = queued.pop_front() {
            if mode == FlushMode::Idle && entry.skip_idle_flush {
                // Not eligible for this drain; keep it for a
                // streaming drain.
                keep_back.push_back(entry);
                continue;
            }
            match entry.take_render() {
                Some(msg) => batch.messages.push(msg),
                None => batch.dropped += 1,
            }
        }

        // Re-insert entries the mode skipped, in their original
        // order, ahead of anything a concurrent push added.
        if !keep_back.is_empty()
            && let Ok(mut q) = self.inner.lock()
        {
            for entry in keep_back.into_iter().rev() {
                q.push_front(entry);
            }
        }

        batch
    }

    /// Drain for a streaming flush. Shorthand for
    /// `drain(FlushMode::Streaming)`.
    pub fn drain_streaming(&self) -> DrainBatch {
        self.drain(FlushMode::Streaming)
    }

    /// Drain for an idle flush, if idle flush is enabled. Returns an
    /// empty batch when the caller has disabled idle flush (via
    /// `cancel_idle_flush_scheduling`).
    pub fn drain_idle(&self) -> DrainBatch {
        let enabled = self.idle_flush_enabled.lock().map(|g| *g).unwrap_or(true);
        if !enabled {
            return DrainBatch::default();
        }
        self.drain(FlushMode::Idle)
    }

    /// Disable idle-flush scheduling. Called when the host cancelled
    /// the task that would have consumed the idle batch — the entries
    /// stay queued for the next streaming drain.
    pub fn cancel_idle_flush_scheduling(&self) {
        if let Ok(mut g) = self.idle_flush_enabled.lock() {
            *g = false;
        }
    }

    /// Re-enable idle flush.
    pub fn enable_idle_flush(&self) {
        if let Ok(mut g) = self.idle_flush_enabled.lock() {
            *g = true;
        }
    }

    /// Whether idle flush is enabled.
    pub fn idle_flush_enabled(&self) -> bool {
        self.idle_flush_enabled.lock().map(|g| *g).unwrap_or(true)
    }

    /// Drop every queued entry without rendering. For shutdown.
    pub fn clear(&self) {
        if let Ok(mut q) = self.inner.lock() {
            q.clear();
        }
    }
}

impl Default for YieldQueue {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn an_empty_queue_drains_empty() {
        let q = YieldQueue::new();
        let b = q.drain_streaming();
        assert!(b.is_empty());
        assert_eq!(b.dropped, 0);
    }

    #[test]
    fn a_registered_entry_renders_on_drain() {
        let q = YieldQueue::new();
        q.register("x", false, || Some("hello".to_string()));
        let b = q.drain_streaming();
        assert_eq!(b.messages, vec!["hello".to_string()]);
    }

    #[test]
    fn a_none_render_is_counted_as_dropped() {
        let q = YieldQueue::new();
        q.register("stale", false, || None);
        let b = q.drain_streaming();
        assert!(b.messages.is_empty());
        assert_eq!(b.dropped, 1);
    }

    #[test]
    fn staleness_is_evaluated_at_drain_time_not_register_time() {
        // The thunk reads a flag that flips between register and
        // drain. The drain must see the *current* value.
        let flag = Arc::new(AtomicBool::new(false));
        let q = YieldQueue::new();
        let f = Arc::clone(&flag);
        q.register("diag", false, move || {
            if f.load(Ordering::SeqCst) {
                None
            } else {
                Some("still valid".to_string())
            }
        });
        // Flip the flag *after* register.
        flag.store(true, Ordering::SeqCst);
        let b = q.drain_streaming();
        assert!(
            b.messages.is_empty(),
            "the thunk must see the flipped flag at drain time",
        );
        assert_eq!(b.dropped, 1);
    }

    #[test]
    fn entries_drain_in_registration_order() {
        let q = YieldQueue::new();
        q.register("a", false, || Some("first".to_string()));
        q.register("b", false, || Some("second".to_string()));
        q.register("c", false, || Some("third".to_string()));
        let b = q.drain_streaming();
        assert_eq!(
            b.messages,
            vec!["first".to_string(), "second".to_string(), "third".to_string()],
        );
    }

    #[test]
    fn a_streaming_drain_includes_skip_idle_entries() {
        let q = YieldQueue::new();
        q.register("steer", true, || Some("mid-work".to_string()));
        let b = q.drain_streaming();
        assert_eq!(b.messages, vec!["mid-work".to_string()]);
    }

    #[test]
    fn an_idle_drain_skips_skip_idle_entries_and_keeps_them() {
        let q = YieldQueue::new();
        q.register("steer", true, || Some("mid-work".to_string()));
        q.register("card", false, || Some("idle-ok".to_string()));
        let b = q.drain_idle();
        assert_eq!(b.messages, vec!["idle-ok".to_string()]);
        // The skip-idle entry is still queued.
        assert_eq!(q.len(), 1);
        // And the next streaming drain picks it up.
        let b = q.drain_streaming();
        assert_eq!(b.messages, vec!["mid-work".to_string()]);
        assert!(q.is_empty());
    }

    #[test]
    fn cancel_idle_flush_disables_the_idle_drain() {
        let q = YieldQueue::new();
        q.register("card", false, || Some("x".to_string()));
        q.cancel_idle_flush_scheduling();
        let b = q.drain_idle();
        assert!(b.is_empty(), "idle flush is disabled");
        assert_eq!(q.len(), 1, "the entry is still queued");
        // Streaming still works.
        let b = q.drain_streaming();
        assert_eq!(b.messages, vec!["x".to_string()]);
    }

    #[test]
    fn re_enabling_idle_flush_restores_the_drain() {
        let q = YieldQueue::new();
        q.cancel_idle_flush_scheduling();
        assert!(!q.idle_flush_enabled());
        q.enable_idle_flush();
        assert!(q.idle_flush_enabled());
        q.register("card", false, || Some("y".to_string()));
        let b = q.drain_idle();
        assert_eq!(b.messages, vec!["y".to_string()]);
    }

    #[test]
    fn drain_runs_thunks_outside_the_lock() {
        // A thunk that registers another entry while running must
        // not deadlock. If the drain held the lock across the
        // thunk, this test would hang.
        let q = Arc::new(YieldQueue::new());
        let q2 = Arc::clone(&q);
        q.register("nested", false, move || {
            q2.register("inner", false, || Some("inner".to_string()));
            Some("outer".to_string())
        });
        let b = q.drain_streaming();
        assert_eq!(b.messages, vec!["outer".to_string()]);
        // The inner entry arrived after the drain took its
        // snapshot; it is queued for the next drain.
        assert_eq!(q.len(), 1);
    }

    #[test]
    fn len_counts_queued_entries() {
        let q = YieldQueue::new();
        assert_eq!(q.len(), 0);
        q.register("a", false, || Some("x".to_string()));
        q.register("b", false, || Some("y".to_string()));
        assert_eq!(q.len(), 2);
        let _ = q.drain_streaming();
        assert_eq!(q.len(), 0);
    }

    #[test]
    fn clear_drops_everything() {
        let q = YieldQueue::new();
        q.register("a", false, || Some("x".to_string()));
        q.register("b", false, || Some("y".to_string()));
        q.clear();
        assert!(q.is_empty());
    }

    #[test]
    fn a_mixed_batch_counts_drops_and_messages() {
        let q = YieldQueue::new();
        q.register("a", false, || Some("ok".to_string()));
        q.register("b", false, || None); // stale
        q.register("c", false, || Some("also ok".to_string()));
        q.register("d", false, || None); // stale
        let b = q.drain_streaming();
        assert_eq!(b.messages, vec!["ok".to_string(), "also ok".to_string()]);
        assert_eq!(b.dropped, 2);
    }

    #[test]
    fn drain_batch_is_empty_when_there_are_no_messages() {
        let b = DrainBatch::default();
        assert!(b.is_empty());
    }
}
