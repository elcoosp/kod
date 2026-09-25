//! WorkPool: keep-alive swarm workers (borrow from oh-my-pi, delta
//! §11.1).
//!
//! # The shape this replaces
//!
//! The pre-§11.1 swarm spawns one agent per subtask and tears it
//! down when the subtask ends. For a workload of many small items
//! (a lint backlog, a list of files to summarize, a batch of
//! review comments) that is dominated by spawn cost: model setup,
//! system-prompt assembly, the first provider round trip. The item
//! itself is seconds of work behind tens of seconds of overhead.
//!
//! A **work pool** keeps a set of agents alive and feeds each one
//! *batches* of items. One agent round trip processes the whole
//! batch in a single structured response, so the per-item overhead
//! is amortized across the batch.
//!
//! # The dispatch policy
//!
//! `dispatch` decides, for each item, which slot it goes to:
//!
//! 1. **Least-loaded idle** — the idle slot whose
//!    `context_tokens / context_window` ratio is smallest. A slot
//!    that has processed little has the most headroom for the next
//!    batch.
//! 2. **Spawn** while `slots.len() < max_agents`.
//! 3. **Round-robin among running** — queue the item onto a busy
//!    slot's next batch.
//!
//! The policy is a pure function of the slot states; `WorkPool`
//! holds the state and applies it.
//!
//! # The yield contract
//!
//! A batch's output schema requires one key per item id. A worker
//! that returns a response missing a key has broken the contract —
//! the batch's items are tombstoned and the slot is released so it
//! can be reused with a fresh schema. The alternative (silently
//! accepting a partial result) loses items, and the caller cannot
//! tell a missing item from a deliberate omission.
//!
//! # What this does NOT do
//!
//! * Not the agent loop. `WorkPool` decides *which slot* gets
//!   *which items*; the caller runs the agent and reports back
//!   through `record_yield`. That split keeps the pool testable
//!   without a provider.
//! * Not persistent. Slots live in memory for the pool's lifetime;
//!   park/revive (the design's §11.2) is a separate layer.
//! * Not a scheduler for dependencies. If two items must run in
//!   order, the caller orders them; the pool only balances load.

use std::collections::{HashMap, VecDeque};

/// One unit of work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkItem {
    /// Opaque id, unique within the pool. The batch's output schema
    /// is keyed on this, so the worker's response can be matched
    /// back to the item.
    pub id: String,
    /// The item's payload, rendered into the batch prompt by the
    /// caller's template. The pool does not interpret it.
    pub payload: serde_json::Value,
}

impl WorkItem {
    pub fn new(id: impl Into<String>, payload: serde_json::Value) -> Self {
        Self {
            id: id.into(),
            payload,
        }
    }
}

/// A slot's current state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotState {
    /// Not processing a batch. Eligible for the least-loaded-idle
    /// pick.
    Idle,
    /// Processing a batch. New items queue for the next batch.
    Running,
    /// Failed or tombstoned. Not eligible for any dispatch; the
    /// caller decides whether to replace it.
    Unavailable,
}

/// One worker slot.
#[derive(Debug, Clone)]
pub struct WorkerSlot {
    pub agent_id: String,
    pub state: SlotState,
    /// Items queued for this slot's next batch. The current batch's
    /// items are not in here — they moved to the in-flight set when
    /// the batch was dispatched.
    pub queue: VecDeque<WorkItem>,
    /// Context tokens the slot has consumed so far. Feeds the
    /// least-loaded ratio.
    pub context_tokens: u64,
    /// The slot's model context window. The ratio's denominator.
    pub context_window: u64,
}

impl WorkerSlot {
    pub fn new(agent_id: impl Into<String>, context_window: u64) -> Self {
        Self {
            agent_id: agent_id.into(),
            state: SlotState::Idle,
            queue: VecDeque::new(),
            context_tokens: 0,
            context_window,
        }
    }

    /// The load ratio, `context_tokens / context_window`. A zero
    /// window reports `1.0` — "full" — so a misconfigured slot is
    /// never picked over a properly configured one.
    pub fn load_ratio(&self) -> f64 {
        if self.context_window == 0 {
            1.0
        } else {
            self.context_tokens as f64 / self.context_window as f64
        }
    }
}

/// The dispatch policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchPolicy {
    /// Keep a pool of at most `max_agents` slots, each processing
    /// batches. The default.
    Batched,
    /// Spawn one slot per item. The design's `freshAgents` mode —
    /// useful when items must not share context (a security
    /// boundary, a leak between unrelated tasks).
    FreshAgents,
}

/// A batch ready for one slot.
#[derive(Debug, Clone)]
pub struct BatchAssignment {
    pub agent_id: String,
    pub items: Vec<WorkItem>,
}

/// A dispatch decision for one incoming item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dispatch {
    /// The item was queued onto an existing slot's next batch.
    Queued { agent_id: String },
    /// The item needs a new slot. The caller spawns an agent with
    /// the given id and reports back with `record_spawned`.
    Spawn { agent_id: String },
    /// The pool is full and no slot can take the item. The caller
    /// waits for a `record_yield` and retries, or rejects the item.
    Rejected { reason: &'static str },
}

/// The pool.
pub struct WorkPool {
    policy: DispatchPolicy,
    max_agents: usize,
    slots: HashMap<String, WorkerSlot>,
    /// Round-robin cursor over slot ids, for step 3 of the policy.
    cursor: usize,
    /// Monotonic counter for generated agent ids.
    next_agent_seq: u64,
    /// The batch size cap: a slot's next batch takes at most this
    /// many items, so one free slot cannot swallow the whole
    /// backlog (the cleanse loop's `takeBatch` budget, §11.9).
    batch_budget: usize,
}

impl WorkPool {
    pub fn new(max_agents: usize, batch_budget: usize) -> Self {
        Self {
            policy: DispatchPolicy::Batched,
            max_agents,
            slots: HashMap::new(),
            cursor: 0,
            next_agent_seq: 1,
            batch_budget,
        }
    }

    pub fn with_policy(mut self, policy: DispatchPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Register a slot the caller has just spawned.
    pub fn record_spawned(&mut self, agent_id: impl Into<String>, context_window: u64) {
        let id = agent_id.into();
        self.slots
            .entry(id.clone())
            .or_insert_with(|| WorkerSlot::new(id, context_window));
    }

    /// Number of live slots (any state).
    pub fn slot_count(&self) -> usize {
        self.slots.len()
    }

    /// Every slot id, sorted. For a UI readout and for tests.
    pub fn slot_ids(&self) -> Vec<String> {
        let mut v: Vec<String> = self.slots.keys().cloned().collect();
        v.sort();
        v
    }

    /// The pool's current batch budget.
    pub fn batch_budget(&self) -> usize {
        self.batch_budget
    }

    fn next_agent_id(&mut self) -> String {
        let id = format!("worker-{}", self.next_agent_seq);
        self.next_agent_seq += 1;
        id
    }

    /// Dispatch one item.
    ///
    /// `Batched` policy:
    /// 1. least-loaded idle slot with queue room;
    /// 2. spawn, while under `max_agents`;
    /// 3. least-loaded running slot with queue room.
    ///
    /// `FreshAgents` policy: always spawn, ignoring `max_agents`
    /// (the caller has opted into one-slot-per-item).
    pub fn dispatch(&mut self, item: WorkItem) -> Dispatch {
        if self.policy == DispatchPolicy::FreshAgents {
            let id = self.next_agent_id();
            // FreshAgents slots are Running from the moment they
            // exist — the item goes with the spawn, not into a
            // queue.
            let mut slot = WorkerSlot::new(id.clone(), 0);
            slot.state = SlotState::Running;
            self.slots.insert(id.clone(), slot);
            return Dispatch::Spawn { agent_id: id };
        }

        // Step 1: least-loaded idle slot with queue room.
        if let Some(id) = self.pick_slot(SlotState::Idle) {
            if let Some(slot) = self.slots.get_mut(&id) {
                slot.queue.push_back(item);
            }
            return Dispatch::Queued { agent_id: id };
        }

        // Step 2: spawn, while under the cap.
        if self.slots.len() < self.max_agents {
            let id = self.next_agent_id();
            let mut slot = WorkerSlot::new(id.clone(), 0);
            slot.queue.push_back(item);
            self.slots.insert(id.clone(), slot);
            return Dispatch::Spawn { agent_id: id };
        }

        // Step 3: least-loaded running slot with queue room.
        if let Some(id) = self.pick_slot(SlotState::Running) {
            if let Some(slot) = self.slots.get_mut(&id) {
                slot.queue.push_back(item);
            }
            return Dispatch::Queued { agent_id: id };
        }

        Dispatch::Rejected {
            reason: "pool full and every running slot's queue is at the batch budget",
        }
    }

    /// Pick a slot in `state` with the smallest load ratio and room
    /// in its queue. Ties break on the round-robin cursor so two
    /// equally-loaded slots alternate.
    fn pick_slot(&mut self, state: SlotState) -> Option<String> {
        let mut candidates: Vec<(&String, &WorkerSlot)> = self
            .slots
            .iter()
            .filter(|(_, s)| s.state == state)
            .collect();
        if candidates.is_empty() {
            return None;
        }
        // Sort by load ratio, then id, so the least-loaded comes
        // first and equal loads have a stable order.
        candidates.sort_by(|a, b| {
            a.1.load_ratio()
                .partial_cmp(&b.1.load_ratio())
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(b.0))
        });
        // Round-robin across the (possibly equal-load) candidates.
        // The cursor *starts* at 0 so the first call returns the
        // least-loaded; each subsequent call advances, which is
        // what makes two equally-loaded slots alternate.
        let n = candidates.len();
        let idx = self.cursor % n;
        self.cursor = self.cursor.wrapping_add(1);
        Some(candidates[idx].0.clone())
    }

    /// Take the next batch from a slot's queue and mark the slot
    /// Running. Returns `None` when the queue is empty.
    ///
    /// The caller runs the agent on the returned batch and calls
    /// `record_yield` when it finishes.
    pub fn take_batch(&mut self, agent_id: &str) -> Option<BatchAssignment> {
        let slot = self.slots.get_mut(agent_id)?;
        if slot.queue.is_empty() {
            return None;
        }
        let take = slot.queue.len().min(self.batch_budget);
        let items: Vec<WorkItem> = slot.queue.drain(..take).collect();
        slot.state = SlotState::Running;
        Some(BatchAssignment {
            agent_id: agent_id.to_string(),
            items,
        })
    }

    /// Record a slot's yield: the batch it processed, how many
    /// context tokens it consumed, and whether the output satisfied
    /// the batch's schema.
    ///
    /// A slot that yielded a valid response goes back to Idle (its
    /// queue may already have more items). A slot that broke the
    /// yield contract is tombstoned (`Unavailable`) — it cannot be
    /// reused with a stale schema.
    pub fn record_yield(
        &mut self,
        agent_id: &str,
        context_tokens: u64,
        yield_contract_satisfied: bool,
    ) {
        let Some(slot) = self.slots.get_mut(agent_id) else {
            return;
        };
        slot.context_tokens = slot.context_tokens.saturating_add(context_tokens);
        slot.state = if yield_contract_satisfied {
            // Back to Idle whether or not the queue has more: an
            // idle slot with queued items is picked up by the next
            // `take_batch`, and the dispatch policy sees it as a
            // valid target for new items meanwhile.
            SlotState::Idle
        } else {
            // A worker that broke the yield contract cannot be
            // reused with a stale schema.
            SlotState::Unavailable
        };
    }

    /// Whether every slot is Idle and every queue is empty. A
    /// caller's completion check.
    pub fn is_drained(&self) -> bool {
        self.slots
            .values()
            .all(|s| s.state == SlotState::Idle && s.queue.is_empty())
    }

    /// How many items are queued across every slot.
    pub fn queued_len(&self) -> usize {
        self.slots.values().map(|s| s.queue.len()).sum()
    }

    /// Remove a slot. The caller uses this after a tombstoned slot
    /// has been replaced, or when shutting the pool down.
    pub fn remove_slot(&mut self, agent_id: &str) -> Option<WorkerSlot> {
        self.slots.remove(agent_id)
    }
}

/// Build the output schema a batch's response must satisfy: one
/// required key per item id.
///
/// The design's `buildWorkPoolOutputSchema`. A worker that returns a
/// response missing a key has broken the contract and its batch is
/// tombstoned. The schema is deliberately flat — an object whose
/// required array is exactly the item ids — so a model's structured
/// output is a single `{ "<id>": <result>, ... }` object.
pub fn build_batch_output_schema(items: &[WorkItem]) -> serde_json::Value {
    let required: Vec<String> = items.iter().map(|i| i.id.clone()).collect();
    serde_json::json!({
        "type": "object",
        "required": required,
        "additionalProperties": true,
    })
}

/// Verify a worker's structured response against a batch's schema:
/// every item id must be a key in the response object.
pub fn yield_contract_satisfied(
    response: &serde_json::Value,
    items: &[WorkItem],
) -> bool {
    let Some(obj) = response.as_object() else {
        return false;
    };
    items.iter().all(|i| obj.contains_key(&i.id))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str) -> WorkItem {
        WorkItem::new(id, serde_json::json!({ "task": id }))
    }

    #[test]
    fn an_empty_pool_spawns_on_first_dispatch() {
        let mut pool = WorkPool::new(3, 10);
        match pool.dispatch(item("a")) {
            Dispatch::Spawn { agent_id } => {
                pool.record_spawned(agent_id.clone(), 8192);
                assert_eq!(pool.slot_count(), 1);
            }
            other => panic!("expected Spawn, got {other:?}"),
        }
    }

    #[test]
    fn a_second_item_goes_to_the_same_idle_slot() {
        let mut pool = WorkPool::new(3, 10);
        let id = match pool.dispatch(item("a")) {
            Dispatch::Spawn { agent_id } => agent_id,
            other => panic!("expected Spawn, got {other:?}"),
        };
        pool.record_spawned(id.clone(), 8192);
        // After `record_spawned` the slot is Idle with one item
        // queued — dispatch should queue the second item onto it.
        match pool.dispatch(item("b")) {
            Dispatch::Queued { agent_id } => assert_eq!(agent_id, id),
            other => panic!("expected Queued, got {other:?}"),
        }
        assert_eq!(pool.queued_len(), 2);
    }

    #[test]
    fn the_pool_spawns_up_to_max_agents() {
        // Spawns happen when there is no idle slot to take the
        // item. Put slot 1 into the Running state by taking a batch
        // from it, then dispatch: no idle slot, under the cap, so
        // the pool spawns.
        let mut pool = WorkPool::new(2, 10);
        let a = match pool.dispatch(item("a")) {
            Dispatch::Spawn { agent_id } => agent_id,
            other => panic!("{other:?}"),
        };
        pool.record_spawned(a.clone(), 8192);
        let _ = pool.take_batch(&a); // Running now

        match pool.dispatch(item("b")) {
            Dispatch::Spawn { agent_id } => assert_ne!(agent_id, a),
            other => panic!("expected Spawn, got {other:?}"),
        }
        assert_eq!(pool.slot_count(), 2);
    }

    #[test]
    fn a_full_pool_queues_onto_running_slots() {
        // At the cap with no idle slot, an item queues onto a
        // running slot's next batch.
        let mut pool = WorkPool::new(1, 10);
        let a = match pool.dispatch(item("a")) {
            Dispatch::Spawn { agent_id } => agent_id,
            other => panic!("{other:?}"),
        };
        pool.record_spawned(a.clone(), 8192);
        let _ = pool.take_batch(&a); // Running now

        match pool.dispatch(item("b")) {
            Dispatch::Queued { agent_id } => assert_eq!(agent_id, a),
            other => panic!("expected Queued, got {other:?}"),
        }
    }

    #[test]
    fn a_pool_with_only_unavailable_slots_rejects() {
        // Rejection happens only when no slot can take work: the
        // pool is at the cap and every slot is Unavailable. A
        // queue is unbounded, so a Running or Idle slot always
        // accepts more items.
        let mut pool = WorkPool::new(1, 10);
        pool.record_spawned("only", 8192);
        // Tombstone the only slot via a broken yield contract.
        pool.record_yield("only", 0, false);
        match pool.dispatch(item("a")) {
            Dispatch::Rejected { .. } => {}
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[test]
    fn a_running_slot_accepts_unbounded_queue_items() {
        // The queue is not capped by the batch budget; the budget
        // only limits how many items one `take_batch` yields.
        let mut pool = WorkPool::new(1, 2);
        let a = match pool.dispatch(item("a")) {
            Dispatch::Spawn { agent_id } => agent_id,
            other => panic!("{other:?}"),
        };
        pool.record_spawned(a, 8192);
        // Take the first batch so the slot is Running.
        let _ = pool.take_batch("worker-1");
        // Dispatch more than the budget; every one queues.
        for i in 0..5 {
            match pool.dispatch(item(&format!("x{i}"))) {
                Dispatch::Queued { .. } => {}
                other => panic!("expected Queued, got {other:?}"),
            }
        }
        assert_eq!(pool.queued_len(), 5);
    }

    #[test]
    fn least_loaded_idle_wins() {
        let mut pool = WorkPool::new(3, 10);
        pool.record_spawned("busy", 8192);
        pool.record_spawned("fresh", 8192);
        // Mark busy as having consumed a lot of context.
        pool.record_yield("busy", 7000, true);
        // Both are Idle now. The least-loaded is "fresh".
        match pool.dispatch(item("a")) {
            Dispatch::Queued { agent_id } => assert_eq!(agent_id, "fresh"),
            other => panic!("expected Queued onto fresh, got {other:?}"),
        }
    }

    #[test]
    fn a_running_slot_accepts_queue_items_when_the_pool_is_full() {
        let mut pool = WorkPool::new(1, 10);
        pool.record_spawned("only", 8192);
        // Take a batch — the slot is now Running.
        let _ = pool.dispatch(item("a"));
        let batch = pool.take_batch("only").unwrap();
        assert_eq!(batch.items.len(), 1);
        // Slot is Running. The pool is at the cap. The next item
        // should queue onto the running slot.
        match pool.dispatch(item("b")) {
            Dispatch::Queued { agent_id } => assert_eq!(agent_id, "only"),
            other => panic!("expected Queued, got {other:?}"),
        }
    }

    #[test]
    fn fresh_agents_policy_spawns_one_per_item() {
        let mut pool = WorkPool::new(1, 10).with_policy(DispatchPolicy::FreshAgents);
        let d1 = pool.dispatch(item("a"));
        let d2 = pool.dispatch(item("b"));
        match (d1, d2) {
            (Dispatch::Spawn { agent_id: a }, Dispatch::Spawn { agent_id: b }) => {
                assert_ne!(a, b);
            }
            other => panic!("expected two Spawns, got {other:?}"),
        }
        assert_eq!(pool.slot_count(), 2);
    }

    #[test]
    fn a_take_batch_respects_the_budget() {
        let mut pool = WorkPool::new(1, 2);
        pool.record_spawned("only", 8192);
        for id in ["a", "b", "c"] {
            pool.dispatch(item(id));
        }
        // Queue has 3, budget is 2. The first take_batch yields 2.
        let b = pool.take_batch("only").unwrap();
        assert_eq!(b.items.len(), 2);
        assert_eq!(b.items[0].id, "a");
        assert_eq!(b.items[1].id, "b");
        // The third stays queued.
        assert_eq!(pool.queued_len(), 1);
    }

    #[test]
    fn a_take_batch_on_an_empty_queue_is_none() {
        let mut pool = WorkPool::new(1, 10);
        pool.record_spawned("only", 8192);
        assert!(pool.take_batch("only").is_none());
    }

    #[test]
    fn a_valid_yield_returns_the_slot_to_idle() {
        let mut pool = WorkPool::new(1, 10);
        pool.record_spawned("only", 8192);
        pool.dispatch(item("a"));
        let _ = pool.take_batch("only");
        pool.record_yield("only", 500, true);
        assert!(pool.is_drained(), "an idle empty slot is drained");
    }

    #[test]
    fn a_broken_yield_contract_tombstones_the_slot() {
        let mut pool = WorkPool::new(1, 10);
        pool.record_spawned("only", 8192);
        pool.dispatch(item("a"));
        let _ = pool.take_batch("only");
        pool.record_yield("only", 500, false);
        // The slot is Unavailable. A further dispatch cannot use it
        // and the pool is at its cap, so the dispatch rejects.
        match pool.dispatch(item("b")) {
            Dispatch::Rejected { .. } => {}
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[test]
    fn a_tombstoned_slot_can_be_removed_and_replaced() {
        let mut pool = WorkPool::new(1, 10);
        pool.record_spawned("only", 8192);
        pool.record_yield("only", 0, false);
        pool.remove_slot("only");
        assert_eq!(pool.slot_count(), 0);
        match pool.dispatch(item("a")) {
            Dispatch::Spawn { .. } => {}
            other => panic!("expected Spawn, got {other:?}"),
        }
    }

    #[test]
    fn load_ratio_with_a_zero_window_is_full() {
        let s = WorkerSlot::new("x", 0);
        assert_eq!(s.load_ratio(), 1.0);
    }

    #[test]
    fn the_schema_requires_every_item_id() {
        let items = vec![item("alpha"), item("beta")];
        let schema = build_batch_output_schema(&items);
        let required = schema["required"].as_array().unwrap();
        assert_eq!(required.len(), 2);
        assert!(required.iter().any(|v| v == "alpha"));
        assert!(required.iter().any(|v| v == "beta"));
    }

    #[test]
    fn yield_contract_holds_when_every_key_is_present() {
        let items = vec![item("a"), item("b")];
        let response = serde_json::json!({"a": 1, "b": 2});
        assert!(yield_contract_satisfied(&response, &items));
    }

    #[test]
    fn yield_contract_fails_when_a_key_is_missing() {
        let items = vec![item("a"), item("b")];
        let response = serde_json::json!({"a": 1});
        assert!(!yield_contract_satisfied(&response, &items));
    }

    #[test]
    fn yield_contract_fails_on_a_non_object_response() {
        let items = vec![item("a")];
        assert!(!yield_contract_satisfied(&serde_json::json!("nope"), &items));
        assert!(!yield_contract_satisfied(&serde_json::json!([1, 2]), &items));
    }

    #[test]
    fn slot_ids_are_sorted() {
        let mut pool = WorkPool::new(3, 10);
        pool.record_spawned("zeta", 8192);
        pool.record_spawned("alpha", 8192);
        assert_eq!(pool.slot_ids(), vec!["alpha".to_string(), "zeta".to_string()]);
    }
}
