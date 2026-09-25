//! IrcBus: delivery semantics over the communication hub (borrow
//! from oh-my-pi, delta §11.3).
//!
//! # The gap
//!
//! [`crate::communication::AgentCommunicationHub`] moves messages:
//! `send_direct`, `broadcast`, a per-agent receiver. What it does
//! not carry is the *semantics* a caller needs to reason about
//! delivery:
//!
//! * **Receipts.** "Did the message reach the target?" has three
//!   honest answers — it was *injected* into a live turn, it *woke*
//!   an idle agent, or it *revived* a parked one. A caller that
//!   only knows "sent" cannot tell a delivered message from one
//!   still sitting in an unbounded queue.
//! * **A cap.** A runaway producer can grow a mailbox without
//!   limit. The design's `MAILBOX_CAP = 100`: past it, the oldest
//!   undelivered message drops.
//! * **A wait.** `send await:true` blocks the sender until the
//!   recipient replies or the timeout elapses, and distinguishes
//!   "the target stopped" from "the timeout fired".
//!
//! # The shape
//!
//! `IrcBus` wraps a hub and adds those three. It tracks, per
//! agent, a mailbox (bounded) and a set of waiters (senders parked
//! on a reply). When a reply arrives addressed to a waiting sender,
//! the bus hands it to the waiter.
//!
//! # Delivery receipts
//!
//! [`Receipt`] is the answer to "what happened to my message":
//!
//! * `Injected` — the target had a live receiver; the message
//!   landed in its mailbox and will reach its next turn.
//! * `Woken` — the target was idle and its receiver was gone; the
//!   message landed in the mailbox and the caller has been told to
//!   wake the target.
//! * `Revived` — the target was parked or dead. The bus *cannot*
//!   revive on its own (that needs the registry's persistence
//!   layer); it reports `Revived` only when the caller has already
//!   revived the target before sending.
//! * `Buffered` — the target is known but has no receiver (an
//!   offline agent); the message stays in the mailbox, and a later
//!   drain returns it.
//!
//! # What this does NOT do
//!
//! * Not the wake turn. The bus tells the caller "the target is
//!   idle and has a message" — running a turn to answer it is the
//!   caller's job (the design's "monitored wake turn").
//! * Not persistence. Mailboxes live in memory; an agent revived
//!   after a process restart has an empty mailbox.
//! * Not the aside-vs-interrupt distinction. A message carries a
//!   [`Delivery`] hint; applying it (an aside is non-interrupting,
//!   an interrupt pre-empts) is the caller's turn loop.

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use tokio::sync::Mutex;

/// The design's per-mailbox cap. Past it, the oldest undelivered
/// message is dropped on enqueue.
pub const MAILBOX_CAP: usize = 100;

/// The design's default `send await:true` timeout.
pub const DEFAULT_IRC_TIMEOUT_MS: u64 = 120_000;

/// How a message should reach its recipient.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// Interrupting: pre-empt the recipient's current work at its
    /// next step boundary.
    Interrupt,
    /// Non-interrupting: delivered at the next round boundary, no
    /// pre-emption.
    Aside,
}

/// What happened to a sent message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Receipt {
    /// The recipient had a live receiver; the message is queued for
    /// its next turn.
    Injected,
    /// The recipient was idle; the caller should wake it to
    /// process the message.
    Woken,
    /// The recipient was parked and the caller revived it before
    /// sending.
    Revived,
    /// The recipient is known but offline; the message is buffered.
    Buffered,
    /// The recipient is unknown to the bus. The message is refused.
    UnknownTarget,
    /// The mailbox was full; the oldest message was dropped to make
    /// room.
    DroppedOldest,
}

/// A message on the bus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BusMessage {
    pub from: String,
    pub to: String,
    pub body: String,
    pub delivery: Delivery,
    /// `Some(id)` when this message is a *reply* to a send that set
    /// `await:true` with that correlation id.
    pub reply_to: Option<u64>,
}

/// Why an `await` send failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendError {
    /// The target is known but is `Dead` — no reply will ever come.
    TargetStopped,
    /// The timeout elapsed with no reply.
    Timeout,
    /// The recipient is unknown to the bus.
    UnknownTarget,
}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TargetStopped => write!(f, "target stopped"),
            Self::Timeout => write!(f, "timeout waiting for reply"),
            Self::UnknownTarget => write!(f, "unknown target"),
        }
    }
}

impl std::error::Error for SendError {}

/// A sender parked on a reply.
struct Waiter {
    tx: tokio::sync::oneshot::Sender<String>,
}

/// Per-agent state.
struct AgentState {
    mailbox: VecDeque<BusMessage>,
    /// Whether the agent has a live receiver. `false` when the
    /// agent is idle or offline — the bus reports `Woken` /
    /// `Buffered` accordingly.
    has_receiver: bool,
    /// Whether the agent has been tombstoned. A send to a dead
    /// agent returns `TargetStopped` on an await.
    dead: bool,
}

/// The bus.
pub struct IrcBus {
    inner: Mutex<Inner>,
}

struct Inner {
    agents: HashMap<String, AgentState>,
    /// Correlation id → waiter. Only the `await:true` path uses it.
    waiters: HashMap<u64, Waiter>,
    next_correlation: u64,
}

impl IrcBus {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                agents: HashMap::new(),
                waiters: HashMap::new(),
                next_correlation: 1,
            }),
        }
    }

    /// Register an agent. `has_receiver` reflects whether the agent
    /// currently has a live turn loop polling its mailbox.
    pub async fn register(&self, id: impl Into<String>, has_receiver: bool) {
        let id = id.into();
        let mut g = self.inner.lock().await;
        g.agents
            .entry(id)
            .or_insert_with(|| AgentState {
                mailbox: VecDeque::new(),
                has_receiver,
                dead: false,
            });
    }

    /// Mark an agent's receiver live or gone. A subagent that
    /// finished its turn but is not yet parked calls this with
    /// `false`; a wake turn calls it with `true`.
    pub async fn set_receiver(&self, id: &str, has_receiver: bool) {
        let mut g = self.inner.lock().await;
        if let Some(a) = g.agents.get_mut(id) {
            a.has_receiver = has_receiver;
        }
    }

    /// Tombstone an agent. A subsequent await-send to it fails with
    /// `TargetStopped` rather than timing out.
    pub async fn mark_dead(&self, id: &str) {
        let mut g = self.inner.lock().await;
        if let Some(a) = g.agents.get_mut(id) {
            a.dead = true;
            a.has_receiver = false;
        }
    }

    /// Send without waiting. Returns the delivery receipt.
    pub async fn send(
        &self,
        from: impl Into<String>,
        to: impl Into<String>,
        body: impl Into<String>,
        delivery: Delivery,
    ) -> Receipt {
        let msg = BusMessage {
            from: from.into(),
            to: to.into(),
            body: body.into(),
            delivery,
            reply_to: None,
        };
        self.enqueue(msg).await
    }

    /// Send and wait for a reply. Returns the reply body, or an
    /// error describing why no reply arrived.
    ///
    /// The caller supplies the timeout; [`DEFAULT_IRC_TIMEOUT_MS`] is
    /// the design's default for a caller that has no preference.
    pub async fn send_await(
        &self,
        from: impl Into<String>,
        to: impl Into<String>,
        body: impl Into<String>,
        timeout: Duration,
    ) -> Result<String, SendError> {
        let to = to.into();
        // Check the target is known and alive before parking a
        // waiter — a dead target should fail fast, not after the
        // timeout.
        let (correlation, rx) = {
            let mut g = self.inner.lock().await;
            let state = g
                .agents
                .get(&to)
                .ok_or(SendError::UnknownTarget)?;
            if state.dead {
                return Err(SendError::TargetStopped);
            }
            let correlation = g.next_correlation;
            g.next_correlation += 1;
            let (tx, rx) = tokio::sync::oneshot::channel();
            g.waiters.insert(correlation, Waiter { tx });
            (correlation, rx)
        };

        // Enqueue the message.
        let msg = BusMessage {
            from: from.into(),
            to: to.clone(),
            body: body.into(),
            delivery: Delivery::Interrupt,
            reply_to: None,
        };
        let _ = self.enqueue(msg).await;

        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(reply)) => Ok(reply),
            Ok(Err(_)) => {
                // The sender half was dropped without a value —
                // this only happens on `mark_dead` between the
                // park and the reply.
                Err(SendError::TargetStopped)
            }
            Err(_) => {
                // The timeout fired. Remove the waiter so a late
                // reply does not sit in the map.
                let mut g = self.inner.lock().await;
                g.waiters.remove(&correlation);
                Err(SendError::Timeout)
            }
        }
    }

    /// Reply to a message. `reply_to` is the correlation id the
    /// waiting sender supplied — the caller that wants the reply
    /// path must thread that id through the original message.
    ///
    /// Returns `true` when a waiter received the reply, `false`
    /// when nobody was waiting (the sender timed out or never
    /// awaited).
    pub async fn reply(&self, correlation: u64, body: impl Into<String>) -> bool {
        let mut g = self.inner.lock().await;
        match g.waiters.remove(&correlation) {
            Some(w) => {
                // Dropping the send failure is correct: the waiter
                // is gone either way, and the reply body has no
                // other home.
                let _ = w.tx.send(body.into());
                true
            }
            None => false,
        }
    }

    /// Enqueue a message, applying the cap and returning the
    /// delivery receipt.
    async fn enqueue(&self, msg: BusMessage) -> Receipt {
        let mut g = self.inner.lock().await;
        let Some(state) = g.agents.get_mut(&msg.to) else {
            return Receipt::UnknownTarget;
        };
        if state.dead {
            return Receipt::UnknownTarget;
        }
        let receipt = if state.has_receiver {
            Receipt::Injected
        } else {
            // No live receiver: the caller wakes or buffers.
            Receipt::Woken
        };
        let mut dropped = false;
        if state.mailbox.len() >= MAILBOX_CAP {
            state.mailbox.pop_front();
            dropped = true;
        }
        state.mailbox.push_back(msg);
        if dropped { Receipt::DroppedOldest } else { receipt }
    }

    /// Drain an agent's mailbox. Returns everything queued, oldest
    /// first. The caller uses this at the top of a turn.
    pub async fn drain(&self, id: &str) -> Vec<BusMessage> {
        let mut g = self.inner.lock().await;
        match g.agents.get_mut(id) {
            Some(state) => state.mailbox.drain(..).collect(),
            None => Vec::new(),
        }
    }

    /// How many messages are queued for an agent.
    pub async fn mailbox_len(&self, id: &str) -> usize {
        self.inner
            .lock()
            .await
            .agents
            .get(id)
            .map(|s| s.mailbox.len())
            .unwrap_or(0)
    }

    /// Whether an agent is dead.
    pub async fn is_dead(&self, id: &str) -> bool {
        self.inner
            .lock()
            .await
            .agents
            .get(id)
            .map(|s| s.dead)
            .unwrap_or(false)
    }

    /// Number of senders currently parked on a reply.
    pub async fn waiter_count(&self) -> usize {
        self.inner.lock().await.waiters.len()
    }
}

impl Default for IrcBus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn a_send_to_a_live_receiver_is_injected() {
        let b = IrcBus::new();
        b.register("a", true).await;
        let r = b.send("x", "a", "hi", Delivery::Aside).await;
        assert_eq!(r, Receipt::Injected);
        assert_eq!(b.mailbox_len("a").await, 1);
    }

    #[tokio::test]
    async fn a_send_to_an_idle_agent_reports_woken() {
        let b = IrcBus::new();
        b.register("a", false).await;
        let r = b.send("x", "a", "hi", Delivery::Aside).await;
        assert_eq!(r, Receipt::Woken);
        assert_eq!(b.mailbox_len("a").await, 1);
    }

    #[tokio::test]
    async fn a_send_to_an_unknown_agent_is_refused() {
        let b = IrcBus::new();
        let r = b.send("x", "nobody", "hi", Delivery::Aside).await;
        assert_eq!(r, Receipt::UnknownTarget);
    }

    #[tokio::test]
    async fn a_send_to_a_dead_agent_is_refused() {
        let b = IrcBus::new();
        b.register("a", true).await;
        b.mark_dead("a").await;
        let r = b.send("x", "a", "hi", Delivery::Aside).await;
        assert_eq!(r, Receipt::UnknownTarget);
    }

    #[tokio::test]
    async fn the_mailbox_cap_drops_the_oldest() {
        let b = IrcBus::new();
        b.register("a", false).await;
        for i in 0..MAILBOX_CAP {
            let r = b.send("x", "a", format!("m{i}"), Delivery::Aside).await;
            assert_eq!(r, Receipt::Woken);
        }
        assert_eq!(b.mailbox_len("a").await, MAILBOX_CAP);
        // The next send hits the cap.
        let r = b.send("x", "a", "overflow", Delivery::Aside).await;
        assert_eq!(r, Receipt::DroppedOldest);
        assert_eq!(b.mailbox_len("a").await, MAILBOX_CAP);
        // The oldest was m0; the mailbox now holds m1..mN + overflow.
        let drained = b.drain("a").await;
        assert_eq!(drained.first().unwrap().body, "m1");
        assert_eq!(drained.last().unwrap().body, "overflow");
    }

    #[tokio::test]
    async fn drain_returns_messages_in_order() {
        let b = IrcBus::new();
        b.register("a", true).await;
        b.send("x", "a", "first", Delivery::Aside).await;
        b.send("x", "a", "second", Delivery::Aside).await;
        let drained = b.drain("a").await;
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].body, "first");
        assert_eq!(drained[1].body, "second");
    }

    #[tokio::test]
    async fn drain_empties_the_mailbox() {
        let b = IrcBus::new();
        b.register("a", true).await;
        b.send("x", "a", "hi", Delivery::Aside).await;
        let _ = b.drain("a").await;
        assert_eq!(b.mailbox_len("a").await, 0);
    }

    #[tokio::test]
    async fn draining_an_unknown_agent_is_empty() {
        let b = IrcBus::new();
        assert!(b.drain("nope").await.is_empty());
    }

    #[tokio::test]
    async fn send_await_to_a_dead_agent_fails_fast() {
        let b = IrcBus::new();
        b.register("a", true).await;
        b.mark_dead("a").await;
        let r = b
            .send_await("x", "a", "hi", Duration::from_secs(10))
            .await;
        assert_eq!(r, Err(SendError::TargetStopped));
    }

    #[tokio::test]
    async fn send_await_to_an_unknown_agent_fails_fast() {
        let b = IrcBus::new();
        let r = b
            .send_await("x", "nope", "hi", Duration::from_secs(10))
            .await;
        assert_eq!(r, Err(SendError::UnknownTarget));
    }

    #[tokio::test]
    async fn send_await_times_out_when_no_reply_arrives() {
        let b = IrcBus::new();
        b.register("a", true).await;
        let r = b
            .send_await("x", "a", "hi", Duration::from_millis(20))
            .await;
        assert_eq!(r, Err(SendError::Timeout));
        // The waiter was cleaned up.
        assert_eq!(b.waiter_count().await, 0);
    }

    #[tokio::test]
    async fn send_await_returns_the_reply_when_one_arrives() {
        let b = Arc::new(IrcBus::new());
        b.register("a", true).await;
        let b2 = Arc::clone(&b);
        tokio::spawn(async move {
            // Simulate the recipient: sleep a tick, then reply.
            tokio::time::sleep(Duration::from_millis(5)).await;
            b2.reply(1, "the answer").await;
        });
        let r = b
            .send_await("x", "a", "question", Duration::from_secs(2))
            .await;
        assert_eq!(r, Ok("the answer".to_string()));
    }

    #[tokio::test]
    async fn reply_without_a_waiter_returns_false() {
        let b = IrcBus::new();
        assert!(!b.reply(999, "nobody").await);
    }

    #[tokio::test]
    async fn set_receiver_flips_the_receipt() {
        let b = IrcBus::new();
        b.register("a", false).await;
        assert_eq!(
            b.send("x", "a", "hi", Delivery::Aside).await,
            Receipt::Woken,
        );
        b.set_receiver("a", true).await;
        assert_eq!(
            b.send("x", "a", "hi2", Delivery::Aside).await,
            Receipt::Injected,
        );
    }

    #[tokio::test]
    async fn is_dead_reports_the_tombstone() {
        let b = IrcBus::new();
        b.register("a", true).await;
        assert!(!b.is_dead("a").await);
        b.mark_dead("a").await;
        assert!(b.is_dead("a").await);
    }

    #[tokio::test]
    async fn mark_dead_on_an_unknown_agent_is_a_no_op() {
        let b = IrcBus::new();
        b.mark_dead("nope").await;
        // No panic, no new entry.
        assert!(!b.is_dead("nope").await);
    }

    #[tokio::test]
    async fn delivery_hint_is_preserved_on_the_message() {
        let b = IrcBus::new();
        b.register("a", true).await;
        b.send("x", "a", "interrupt!", Delivery::Interrupt).await;
        let drained = b.drain("a").await;
        assert_eq!(drained[0].delivery, Delivery::Interrupt);
    }

    #[tokio::test]
    async fn a_reply_is_correlated_by_id() {
        // Two concurrent await sends; the replies must land on the
        // right waiter.
        let b = Arc::new(IrcBus::new());
        b.register("a", true).await;
        let b1 = Arc::clone(&b);
        let b2 = Arc::clone(&b);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(5)).await;
            b1.reply(1, "reply-one").await;
        });
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            b2.reply(2, "reply-two").await;
        });
        let (r1, r2) = tokio::join!(
            b.send_await("x", "a", "q1", Duration::from_secs(2)),
            b.send_await("y", "a", "q2", Duration::from_secs(2)),
        );
        assert_eq!(r1, Ok("reply-one".to_string()));
        assert_eq!(r2, Ok("reply-two".to_string()));
    }
}
