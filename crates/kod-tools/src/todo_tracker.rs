//! TodoTracker: nudge machinery for the todo tool (borrow from
//! oh-my-pi, delta §11.7).
//!
//! # The gap
//!
//! The `todo` tool exists and the engine notes evidence against an
//! in-progress item. What it does *not* do is schedule the agent back
//! onto its todos. A model that stops writing to its list for a while
//! — or stops entirely with items still open — gets no reminder.
//!
//! # Three nudges
//!
//! [`TodoTracker`] answers three questions a turn loop asks:
//!
//! 1. **Prelude.** At the first turn, should the model be offered the
//!    todo tool? Yes unless the prompt is a question (`?`) or an
//!    exclamation (`!`), or todos already exist.
//! 2. **Mid-run.** After N successful *mutating* tool calls with no
//!    intervening todo touch, is a reconcile nudge due? Bounded so it
//!    fires at most twice per cycle.
//! 3. **Completion.** When the agent stops with incomplete todos,
//!    should it be reminded? Skipped when the last assistant line is a
//!    question (the model is asking, not done) or async wakes are
//!    pending (the model is waiting, not stopped).
//!
//! # The latch
//!
//! A completion reminder sets a `reminder_awaiting_progress` latch.
//! While it is set, a second reminder is suppressed until a tool call
//! makes progress — the model has been told once; telling it again
//! without new information is noise.
//!
//! # What this is NOT
//!
//! * Not the todo store. [`crate::todo::TodoList`] owns the items;
//!   this tracks nudges.
//! * Not the injection. The tracker decides *whether* a nudge is due;
//!   the engine renders and injects it.

/// The design's mid-run threshold: this many successful mutating
/// tool calls without a todo touch earns one reconcile nudge.
pub const NUDGE_AFTER_MUTATING_CALLS: u32 = 12;

/// The design's cap: at most this many mid-run nudges per cycle.
pub const MAX_NUDGES_PER_CYCLE: u32 = 2;

/// What the tracker decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nudge {
    /// Nothing is due.
    None,
    /// Offer the todo tool at the start of the first turn.
    Prelude,
    /// Ask the model to reconcile its list after a run of mutating
    /// calls.
    MidRun,
    /// Remind the model that todos are still open as it stops.
    CompletionReminder,
}

/// The tracker.
#[derive(Debug, Default)]
pub struct TodoTracker {
    /// Successful mutating tool calls since the last todo touch.
    mutating_since_todo: u32,
    /// Mid-run nudges fired this cycle.
    nudges_this_cycle: u32,
    /// Whether a completion reminder has been issued and no progress
    /// has happened since.
    reminder_awaiting_progress: bool,
    /// Whether the prelude has already been offered once.
    prelude_offered: bool,
}

impl TodoTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Should the first turn offer the todo tool?
    ///
    /// `input` is the user prompt; `has_todos` is whether the list is
    /// non-empty already. A prompt that ends in `?` or `!` is a
    /// question or an exclamation — not a task to plan, so the tool is
    /// not pushed.
    pub fn should_offer_prelude(&self, input: &str, has_todos: bool) -> bool {
        if self.prelude_offered {
            return false;
        }
        if has_todos {
            return false;
        }
        let last = input.trim_end().chars().last();
        !matches!(last, Some('?') | Some('!'))
    }

    /// Mark that the prelude was offered. Called once by the engine
    /// after it injects the prelude.
    pub fn note_prelude_offered(&mut self) {
        self.prelude_offered = true;
    }

    /// Record one tool call. `is_mutating` — the tool writes or
    /// executes. `is_todo_touch` — the tool *is* the todo tool.
    ///
    /// A todo touch resets the mutating counter and clears the
    /// completion latch: the model is engaging with its plan, which
    /// is exactly what the nudges were asking for.
    pub fn observe_tool(&mut self, is_mutating: bool, is_todo_touch: bool) {
        if is_todo_touch {
            self.mutating_since_todo = 0;
            self.nudges_this_cycle = 0;
            self.reminder_awaiting_progress = false;
            return;
        }
        if is_mutating {
            self.mutating_since_todo = self.mutating_since_todo.saturating_add(1);
        }
    }

    /// Is a mid-run reconcile nudge due?
    ///
    /// True when the mutating counter has reached the threshold and
    /// the per-cycle cap is not spent. Consuming the nudge increments
    /// the cycle counter.
    pub fn take_mid_run_nudge(&mut self) -> bool {
        if self.mutating_since_todo < NUDGE_AFTER_MUTATING_CALLS {
            return false;
        }
        if self.nudges_this_cycle >= MAX_NUDGES_PER_CYCLE {
            return false;
        }
        self.mutating_since_todo = 0;
        self.nudges_this_cycle += 1;
        true
    }

    /// Is a completion reminder due?
    ///
    /// `incomplete_todos` — there is at least one open item.
    /// `last_line_is_question` — the model's final line ends in `?`
    /// (it is asking, not finishing). `async_wakes_pending` — a
    /// background job will deliver a result, so the model is waiting,
    /// not stopped.
    pub fn completion_reminder_due(
        &self,
        incomplete_todos: bool,
        last_line_is_question: bool,
        async_wakes_pending: bool,
    ) -> bool {
        if !incomplete_todos {
            return false;
        }
        if self.reminder_awaiting_progress {
            return false;
        }
        if last_line_is_question {
            return false;
        }
        if async_wakes_pending {
            return false;
        }
        true
    }

    /// Record that the completion reminder was issued. Sets the latch
    /// so a second reminder is suppressed until a tool call makes
    /// progress.
    pub fn note_completion_reminder(&mut self) {
        self.reminder_awaiting_progress = true;
    }

    /// The current mutating-call count.
    pub fn mutating_since_todo(&self) -> u32 {
        self.mutating_since_todo
    }

    /// The mid-run nudges fired this cycle.
    pub fn nudges_this_cycle(&self) -> u32 {
        self.nudges_this_cycle
    }

    /// Whether a completion reminder is outstanding.
    pub fn reminder_awaiting_progress(&self) -> bool {
        self.reminder_awaiting_progress
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- prelude ------------------------------------------------------

    #[test]
    fn a_task_prompt_offers_the_prelude() {
        let t = TodoTracker::new();
        assert!(t.should_offer_prelude("add a feature", false));
    }

    #[test]
    fn a_question_does_not_offer_the_prelude() {
        let t = TodoTracker::new();
        assert!(!t.should_offer_prelude("what does this do?", false));
    }

    #[test]
    fn an_exclamation_does_not_offer_the_prelude() {
        let t = TodoTracker::new();
        assert!(!t.should_offer_prelude("do it now!", false));
    }

    #[test]
    fn existing_todos_suppress_the_prelude() {
        let t = TodoTracker::new();
        assert!(!t.should_offer_prelude("add a feature", true));
    }

    #[test]
    fn the_prelude_is_offered_at_most_once() {
        let mut t = TodoTracker::new();
        assert!(t.should_offer_prelude("task", false));
        t.note_prelude_offered();
        assert!(!t.should_offer_prelude("task", false));
    }

    #[test]
    fn trailing_whitespace_does_not_hide_a_question_mark() {
        let t = TodoTracker::new();
        assert!(!t.should_offer_prelude("what now?   ", false));
    }

    // ---- mid-run nudge ------------------------------------------------

    #[test]
    fn fewer_than_the_threshold_mutating_calls_earn_no_nudge() {
        let mut t = TodoTracker::new();
        for _ in 0..(NUDGE_AFTER_MUTATING_CALLS - 1) {
            t.observe_tool(true, false);
        }
        assert!(!t.take_mid_run_nudge());
    }

    #[test]
    fn the_threshold_earns_a_nudge() {
        let mut t = TodoTracker::new();
        for _ in 0..NUDGE_AFTER_MUTATING_CALLS {
            t.observe_tool(true, false);
        }
        assert!(t.take_mid_run_nudge());
    }

    #[test]
    fn a_non_mutating_call_does_not_count() {
        let mut t = TodoTracker::new();
        for _ in 0..100 {
            t.observe_tool(false, false);
        }
        assert_eq!(t.mutating_since_todo(), 0);
        assert!(!t.take_mid_run_nudge());
    }

    #[test]
    fn a_todo_touch_resets_the_counter() {
        let mut t = TodoTracker::new();
        for _ in 0..10 {
            t.observe_tool(true, false);
        }
        t.observe_tool(false, true);
        assert_eq!(t.mutating_since_todo(), 0);
    }

    #[test]
    fn the_nudge_cap_is_respected() {
        let mut t = TodoTracker::new();
        for cycle in 0..4 {
            for _ in 0..NUDGE_AFTER_MUTATING_CALLS {
                t.observe_tool(true, false);
            }
            let fired = t.take_mid_run_nudge();
            if cycle < MAX_NUDGES_PER_CYCLE {
                assert!(fired, "nudge {cycle} should fire");
            } else {
                assert!(!fired, "nudge {cycle} should be capped");
            }
        }
        assert_eq!(t.nudges_this_cycle(), MAX_NUDGES_PER_CYCLE);
    }

    #[test]
    fn a_todo_touch_resets_the_cycle_cap() {
        let mut t = TodoTracker::new();
        for _ in 0..2 {
            for _ in 0..NUDGE_AFTER_MUTATING_CALLS {
                t.observe_tool(true, false);
            }
            assert!(t.take_mid_run_nudge());
        }
        // Cap reached. A todo touch resets it.
        t.observe_tool(false, true);
        assert_eq!(t.nudges_this_cycle(), 0);
        for _ in 0..NUDGE_AFTER_MUTATING_CALLS {
            t.observe_tool(true, false);
        }
        assert!(t.take_mid_run_nudge(), "a fresh cycle allows a fresh nudge");
    }

    // ---- completion reminder -----------------------------------------

    #[test]
    fn incomplete_todos_at_stop_earn_a_reminder() {
        let t = TodoTracker::new();
        assert!(t.completion_reminder_due(true, false, false));
    }

    #[test]
    fn complete_todos_earn_no_reminder() {
        let t = TodoTracker::new();
        assert!(!t.completion_reminder_due(false, false, false));
    }

    #[test]
    fn a_question_ending_suppresses_the_reminder() {
        let t = TodoTracker::new();
        assert!(!t.completion_reminder_due(true, true, false));
    }

    #[test]
    fn pending_async_wakes_suppress_the_reminder() {
        let t = TodoTracker::new();
        assert!(!t.completion_reminder_due(true, false, true));
    }

    #[test]
    fn the_reminder_latch_suppresses_a_second_reminder() {
        let mut t = TodoTracker::new();
        assert!(t.completion_reminder_due(true, false, false));
        t.note_completion_reminder();
        assert!(!t.completion_reminder_due(true, false, false));
    }

    #[test]
    fn a_tool_touch_clears_the_reminder_latch() {
        let mut t = TodoTracker::new();
        t.note_completion_reminder();
        assert!(t.reminder_awaiting_progress());
        t.observe_tool(false, true);
        assert!(!t.reminder_awaiting_progress());
    }

    #[test]
    fn a_non_todo_tool_does_not_clear_the_latch() {
        // A mutating call is progress the nudge asked for only when
        // it touches the todo list; a raw write does not clear the
        // completion latch.
        let mut t = TodoTracker::new();
        t.note_completion_reminder();
        t.observe_tool(true, false);
        assert!(t.reminder_awaiting_progress());
    }
}
