//! Budgeted goals runtime (borrow from oh-my-pi, delta §11.6).
//!
//! # What this is
//!
//! One active objective per session, with a token budget and a
//! wall-clock budget. The goal runtime answers three questions the
//! existing goal loop does not:
//!
//! * **How much has this goal cost?** Not "how much has the
//!   session cost" — a goal that started ten turns in has its own
//!   accounting. The delta is `Δinput + Δcache_write + Δoutput`
//!   over the goal's lifetime. `cacheRead` is excluded: a token
//!   served from cache was already paid for by the write that put
//!   it there, and counting it again would double-bill a
//!   long-running prefix.
//! * **Is the budget spent?** When the accumulated cost crosses the
//!   goal's token or time budget, the status flips to
//!   `BudgetLimited` and the caller emits one steer message — once,
//!   deduped — rather than continuing to spend.
//! * **What happens after a stop?** A goal that is still `Active`
//!   when the agent stops gets a hidden continuation message; a goal
//!   the user paused stays paused; a completed goal reports its
//!   final budget.
//!
//! # Why `cacheWrite` is counted
//!
//! The design is explicit: `cacheRead` is free (the prefix it
//! describes was paid for once) but `cacheWrite` is not. A one-hour
//! cache rotation writes 100K+ tokens, and those tokens bill at the
//! write rate. A budget that ignored them would undercount a long
//! session by exactly the amount that makes it expensive.
//!
//! # What this does NOT do
//!
//! * Not the loop. The runtime tracks a goal's state; running turns
//!   is the engine's job.
//! * Not persistence. A goal lives in memory for the session; a
//!   caller that wants a goal to survive a restart writes the
//!   `Goal` out itself.
//! * Not a scheduler. One goal at a time — starting a new one
//!   replaces the old (with the old's status set to `Dropped`).

use kod_provider::TokenUsage;

/// The lifecycle of a goal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    /// Being worked on.
    Active,
    /// The token or time budget was exhausted. The goal is not
    /// dropped — the user can raise the budget — but the runtime
    /// stops spending.
    BudgetLimited,
    /// The user paused it. Resuming clears the pause.
    Paused,
    /// The model declared it met.
    Complete,
    /// Replaced by a new goal, or explicitly dropped.
    Dropped,
}

impl GoalStatus {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Complete | Self::Dropped)
    }

    /// Whether the goal should keep consuming turns.
    pub fn is_running(self) -> bool {
        matches!(self, Self::Active)
    }
}

/// Budget for a goal, applied at `start` time. Both fields are
/// optional; `None` means "no cap on this dimension".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GoalBudget {
    /// Token cap. Counted as `Δ(input − cache_read) + output` per
    /// turn (see [`Goal::delta_tokens`]).
    pub token: Option<u64>,
    /// Wall-clock cap, in seconds.
    pub time_seconds: Option<u64>,
}

/// One objective.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Goal {
    pub id: u64,
    /// The objective text, as the user stated it.
    pub objective: String,
    pub status: GoalStatus,
    /// `None` means no token cap; the goal runs until the model
    /// declares it met or the caller drops it.
    pub token_budget: Option<u64>,
    /// Accumulated across the goal's turns.
    pub tokens_used: u64,
    /// `None` means no wall-clock cap.
    pub time_budget_seconds: Option<u64>,
    pub time_used_seconds: u64,
    /// Whether the budget-limited steer has already been emitted.
    /// The message is once per goal, not once per turn — the design
    /// dedupes it.
    pub budget_steer_emitted: bool,
}

impl Goal {
    pub fn new(id: u64, objective: impl Into<String>) -> Self {
        Self {
            id,
            objective: objective.into(),
            status: GoalStatus::Active,
            token_budget: None,
            tokens_used: 0,
            time_budget_seconds: None,
            time_used_seconds: 0,
            budget_steer_emitted: false,
        }
    }

    pub fn with_token_budget(mut self, tokens: u64) -> Self {
        self.token_budget = Some(tokens);
        self
    }

    pub fn with_time_budget(mut self, seconds: u64) -> Self {
        self.time_budget_seconds = Some(seconds);
        self
    }

    /// The new work one turn represents, in tokens.
    ///
    /// `prompt_tokens` is the whole input window the provider
    /// processed; `cache_read_tokens` is the portion served from the
    /// provider's cache — that portion was paid for by an earlier
    /// write and is not new work now. Everything else in the window
    /// is new this turn, including any tokens that were *written* to
    /// cache (a written token is new; the write premium is a cost
    /// concern, not a token-budget one).
    ///
    /// So: `delta = (prompt − cache_read) + output`. `cache_write`
    /// is not added separately — it is already inside the
    /// "new input" remainder.
    pub fn delta_tokens(usage: &TokenUsage) -> u64 {
        let new_input =
            (usage.prompt_tokens as u64).saturating_sub(usage.cache_read_tokens.unwrap_or(0));
        let output = usage.completion_tokens as u64;
        new_input + output
    }

    /// Account one turn's usage. A terminal goal ignores the
    /// observation — a complete or dropped goal must not change
    /// state.
    pub fn observe_usage(&mut self, usage: &TokenUsage) {
        if self.status.is_terminal() {
            return;
        }
        self.tokens_used = self.tokens_used.saturating_add(Self::delta_tokens(usage));
        self.recheck_budget();
    }

    /// Account `seconds` of wall time.
    pub fn observe_time(&mut self, seconds: u64) {
        if self.status.is_terminal() {
            return;
        }
        self.time_used_seconds = self.time_used_seconds.saturating_add(seconds);
        self.recheck_budget();
    }

    /// Flip to `BudgetLimited` when either budget is exhausted. A
    /// paused goal is not affected — the user paused it, not the
    /// budget.
    fn recheck_budget(&mut self) {
        if self.status != GoalStatus::Active {
            return;
        }
        let tokens_over = self
            .token_budget
            .map(|b| self.tokens_used >= b)
            .unwrap_or(false);
        let time_over = self
            .time_budget_seconds
            .map(|b| self.time_used_seconds >= b)
            .unwrap_or(false);
        if tokens_over || time_over {
            self.status = GoalStatus::BudgetLimited;
        }
    }

    /// Whether the caller should emit the budget-limited steer. True
    /// once per goal.
    pub fn take_budget_steer(&mut self) -> bool {
        if self.status == GoalStatus::BudgetLimited && !self.budget_steer_emitted {
            self.budget_steer_emitted = true;
            return true;
        }
        false
    }

    /// Whether the runtime wants a continuation turn: the goal is
    /// still active, and the agent has stopped.
    pub fn wants_continuation(&self) -> bool {
        self.status == GoalStatus::Active
    }

    /// A one-line budget report for the end of a completed goal.
    pub fn report(&self) -> String {
        let token_part = match self.token_budget {
            Some(b) => format!("{} / {} tokens", self.tokens_used, b),
            None => format!("{} tokens", self.tokens_used),
        };
        let time_part = match self.time_budget_seconds {
            Some(b) => format!("{} / {} s", self.time_used_seconds, b),
            None => format!("{} s", self.time_used_seconds),
        };
        format!("goal {:?}: {token_part}, {time_part}", self.status,)
    }
}

/// The session's goal runtime: at most one goal.
#[derive(Debug, Default)]
pub struct GoalRuntime {
    current: Option<Goal>,
    next_id: u64,
}

impl GoalRuntime {
    pub fn new() -> Self {
        Self {
            current: None,
            next_id: 1,
        }
    }

    /// Start a new goal, dropping any previous one. Returns the new
    /// goal's id.
    pub fn start(&mut self, objective: impl Into<String>) -> u64 {
        if let Some(prev) = self.current.as_mut()
            && !prev.status.is_terminal()
        {
            prev.status = GoalStatus::Dropped;
        }
        let id = self.next_id;
        self.next_id += 1;
        self.current = Some(Goal::new(id, objective));
        id
    }

    /// Start a new goal with a budget. Same as [`Self::start`] but
    /// attaches the caller's caps to the fresh goal, so the runtime
    /// can flip to `BudgetLimited` on a later `observe_usage` /
    /// `observe_time` without a separate setter call.
    pub fn start_with(&mut self, objective: impl Into<String>, budget: GoalBudget) -> u64 {
        if let Some(prev) = self.current.as_mut()
            && !prev.status.is_terminal()
        {
            prev.status = GoalStatus::Dropped;
        }
        let id = self.next_id;
        self.next_id += 1;
        let mut g = Goal::new(id, objective);
        g.token_budget = budget.token;
        g.time_budget_seconds = budget.time_seconds;
        self.current = Some(g);
        id
    }

    /// The current goal, if any.
    pub fn current(&self) -> Option<&Goal> {
        self.current.as_ref()
    }

    pub fn current_mut(&mut self) -> Option<&mut Goal> {
        self.current.as_mut()
    }

    /// Account a turn's usage against the current goal. No-op when
    /// there is no goal.
    pub fn observe_usage(&mut self, usage: &TokenUsage) {
        if let Some(g) = self.current.as_mut() {
            g.observe_usage(usage);
        }
    }

    /// Account wall time.
    pub fn observe_time(&mut self, seconds: u64) {
        if let Some(g) = self.current.as_mut() {
            g.observe_time(seconds);
        }
    }

    /// Pause the current goal. No-op for a terminal or already-paused
    /// goal.
    pub fn pause(&mut self) {
        if let Some(g) = self.current.as_mut()
            && g.status == GoalStatus::Active
        {
            g.status = GoalStatus::Paused;
        }
    }

    /// Resume a paused goal. No-op for any other status.
    pub fn resume(&mut self) {
        if let Some(g) = self.current.as_mut()
            && g.status == GoalStatus::Paused
        {
            g.status = GoalStatus::Active;
        }
    }

    /// Mark the current goal complete.
    pub fn complete(&mut self) {
        if let Some(g) = self.current.as_mut()
            && !g.status.is_terminal()
        {
            g.status = GoalStatus::Complete;
        }
    }

    /// Drop the current goal.
    pub fn drop_current(&mut self) {
        if let Some(g) = self.current.as_mut() {
            g.status = GoalStatus::Dropped;
        }
    }

    /// Whether a continuation turn should fire: a goal exists and
    /// wants one.
    pub fn wants_continuation(&self) -> bool {
        self.current
            .as_ref()
            .map(|g| g.wants_continuation())
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(prompt: usize, cache_read: u64, cache_write: u64, completion: usize) -> TokenUsage {
        TokenUsage {
            prompt_tokens: prompt,
            completion_tokens: completion,
            total_tokens: prompt + completion,
            cache_read_tokens: Some(cache_read),
            cache_creation_tokens: Some(cache_write),
            cache_creation_1h_tokens: None,
        }
    }

    // ---- delta accounting ---------------------------------------------

    #[test]
    fn delta_is_new_input_plus_output() {
        // prompt 1000, of which 800 from cache-read; 50 output.
        // New input = 1000 - 800 = 200; delta = 200 + 50 = 250.
        let u = usage(1000, 800, 0, 50);
        assert_eq!(Goal::delta_tokens(&u), 250);
    }

    #[test]
    fn delta_excludes_cache_read_volume() {
        // Hold the *new* input fixed and vary cache_read: the delta
        // is unchanged, because a cache read is not new work.
        let a = usage(1000, 0, 0, 50); // new input 1000
        let b = usage(1900, 900, 0, 50); // new input 1000
        assert_eq!(Goal::delta_tokens(&a), Goal::delta_tokens(&b));
    }

    #[test]
    fn delta_does_not_add_a_cache_write_premium() {
        // A token that was written to cache is still new this turn.
        // The write premium is a cost concern, not a token-budget
        // one, so the delta is unchanged whether the token was
        // written or not.
        let unwritten = usage(1000, 0, 0, 50); // new input 1000
        let written = usage(1000, 0, 500, 50); // new input 1000
        assert_eq!(Goal::delta_tokens(&unwritten), Goal::delta_tokens(&written),);
    }

    // ---- budget accounting --------------------------------------------

    #[test]
    fn tokens_accumulate_across_turns() {
        let mut g = Goal::new(1, "do the thing");
        g.observe_usage(&usage(100, 0, 0, 10)); // 110
        g.observe_usage(&usage(100, 0, 0, 10)); // 110
        assert_eq!(g.tokens_used, 220);
    }

    #[test]
    fn a_token_budget_flips_to_budget_limited() {
        let mut g = Goal::new(1, "x").with_token_budget(200);
        g.observe_usage(&usage(100, 0, 0, 10)); // 110 — under
        assert_eq!(g.status, GoalStatus::Active);
        g.observe_usage(&usage(100, 0, 0, 10)); // 220 — over
        assert_eq!(g.status, GoalStatus::BudgetLimited);
    }

    #[test]
    fn a_time_budget_flips_to_budget_limited() {
        let mut g = Goal::new(1, "x").with_time_budget(10);
        g.observe_time(5);
        assert_eq!(g.status, GoalStatus::Active);
        g.observe_time(5);
        assert_eq!(g.status, GoalStatus::BudgetLimited);
    }

    #[test]
    fn no_budget_means_never_budget_limited() {
        let mut g = Goal::new(1, "x");
        for _ in 0..100 {
            g.observe_usage(&usage(1_000_000, 0, 0, 1000));
        }
        assert_eq!(g.status, GoalStatus::Active);
    }

    #[test]
    fn a_terminal_goal_ignores_observations() {
        let mut g = Goal::new(1, "x").with_token_budget(100);
        g.status = GoalStatus::Complete;
        g.observe_usage(&usage(1000, 0, 0, 100));
        assert_eq!(g.tokens_used, 0, "a complete goal does not accumulate");
    }

    #[test]
    fn the_budget_steer_fires_once() {
        let mut g = Goal::new(1, "x").with_token_budget(100);
        g.observe_usage(&usage(200, 0, 0, 0));
        assert!(g.take_budget_steer());
        assert!(!g.take_budget_steer(), "the steer is once per goal");
    }

    #[test]
    fn the_budget_steer_does_not_fire_when_active() {
        let mut g = Goal::new(1, "x").with_token_budget(1000);
        g.observe_usage(&usage(100, 0, 0, 0));
        assert!(!g.take_budget_steer());
    }

    #[test]
    fn wants_continuation_only_while_active() {
        let mut g = Goal::new(1, "x");
        assert!(g.wants_continuation());
        g.status = GoalStatus::Paused;
        assert!(!g.wants_continuation());
        g.status = GoalStatus::BudgetLimited;
        assert!(!g.wants_continuation());
    }

    // ---- runtime ------------------------------------------------------

    #[test]
    fn starting_a_second_goal_drops_the_first() {
        let mut rt = GoalRuntime::new();
        let id1 = rt.start("first");
        let id2 = rt.start("second");
        assert_ne!(id1, id2);
        assert_eq!(rt.current().unwrap().id, id2);
        assert_eq!(rt.current().unwrap().objective, "second");
    }

    #[test]
    fn pause_and_resume() {
        let mut rt = GoalRuntime::new();
        rt.start("x");
        rt.pause();
        assert_eq!(rt.current().unwrap().status, GoalStatus::Paused);
        rt.resume();
        assert_eq!(rt.current().unwrap().status, GoalStatus::Active);
    }

    #[test]
    fn resume_is_a_no_op_on_an_active_goal() {
        let mut rt = GoalRuntime::new();
        rt.start("x");
        rt.resume();
        assert_eq!(rt.current().unwrap().status, GoalStatus::Active);
    }

    #[test]
    fn complete_marks_the_goal() {
        let mut rt = GoalRuntime::new();
        rt.start("x");
        rt.complete();
        assert_eq!(rt.current().unwrap().status, GoalStatus::Complete);
    }

    #[test]
    fn the_runtime_accumulates_usage() {
        let mut rt = GoalRuntime::new();
        rt.start("x");
        rt.observe_usage(&usage(100, 0, 0, 10));
        rt.observe_usage(&usage(100, 0, 0, 10));
        assert_eq!(rt.current().unwrap().tokens_used, 220);
    }

    #[test]
    fn wants_continuation_reflects_the_goal() {
        let mut rt = GoalRuntime::new();
        assert!(!rt.wants_continuation(), "no goal, no continuation");
        rt.start("x");
        assert!(rt.wants_continuation());
        rt.pause();
        assert!(!rt.wants_continuation());
    }

    #[test]
    fn a_report_names_the_status_and_totals() {
        let mut g = Goal::new(1, "x").with_token_budget(1000);
        g.observe_usage(&usage(500, 0, 0, 100));
        g.status = GoalStatus::Complete;
        let r = g.report();
        assert!(r.contains("Complete"), "got: {r}");
        assert!(r.contains("600 / 1000 tokens"), "got: {r}");
    }
}

#[cfg(test)]
mod start_with_budget_tests {
    use super::*;

    #[test]
    fn start_with_attaches_both_caps() {
        let mut rt = GoalRuntime::new();
        let id = rt.start_with(
            "ship it",
            GoalBudget {
                token: Some(500),
                time_seconds: Some(60),
            },
        );
        let g = rt.current().expect("a goal exists");
        assert_eq!(g.id, id);
        assert_eq!(g.token_budget, Some(500));
        assert_eq!(g.time_budget_seconds, Some(60));
        assert_eq!(g.status, GoalStatus::Active);
        assert_eq!(g.tokens_used, 0);
        assert_eq!(g.time_used_seconds, 0);
    }

    #[test]
    fn start_with_default_budget_matches_start() {
        let mut rt = GoalRuntime::new();
        rt.start_with("x", GoalBudget::default());
        let g = rt.current().unwrap();
        assert_eq!(g.token_budget, None);
        assert_eq!(g.time_budget_seconds, None);
    }

    #[test]
    fn start_with_can_flip_to_budget_limited_on_first_observation() {
        let mut rt = GoalRuntime::new();
        rt.start_with(
            "x",
            GoalBudget {
                token: Some(100),
                time_seconds: None,
            },
        );
        let u = TokenUsage {
            prompt_tokens: 200,
            completion_tokens: 0,
            ..Default::default()
        };
        rt.observe_usage(&u);
        assert_eq!(rt.current().unwrap().status, GoalStatus::BudgetLimited);
        assert!(rt.current_mut().unwrap().take_budget_steer());
    }
}

#[cfg(test)]
mod goals_proptests {
    //! Properties for `Goal`'s state machine.
    //!
    //! The 21 example tests pin specific cases: delta excludes
    //! cache-read, tokens accumulate, a tiny budget flips to
    //! `BudgetLimited`, the steer fires once, terminal goals ignore
    //! observations. These properties pin the *state-machine
    //! invariants* across arbitrary sequences of observations:
    //!
    //! * monotone counters (never decrease);
    //! * terminal status is a hard stop for observations;
    //! * `BudgetLimited` is a one-way door under observations;
    //! * `take_budget_steer` fires exactly once;
    //! * `wants_continuation` iff `status == Active`.
    //!
    //! All arithmetic in `delta_tokens` is `saturating_*`, so the
    //! saturation property is a regression guard, not a bug probe.

    use super::*;
    use proptest::prelude::*;

    fn usage(prompt: usize, cache_read: Option<u64>, completion: usize) -> TokenUsage {
        TokenUsage {
            prompt_tokens: prompt,
            completion_tokens: completion,
            total_tokens: prompt.saturating_add(completion),
            cache_read_tokens: cache_read,
            cache_creation_tokens: None,
            cache_creation_1h_tokens: None,
        }
    }

    fn arb_usage() -> impl Strategy<Value = TokenUsage> {
        (
            0usize..=1_000_000usize,
            prop::option::of(0u64..=1_000_000u64),
            0usize..=1_000_000usize,
        )
            .prop_map(|(p, cr, c)| usage(p, cr, c))
    }

    fn arb_usage_seq() -> impl Strategy<Value = Vec<TokenUsage>> {
        prop::collection::vec(arb_usage(), 0..=6)
    }

    fn any_terminal() -> impl Strategy<Value = GoalStatus> {
        prop::sample::select(vec![GoalStatus::Complete, GoalStatus::Dropped])
    }

    proptest! {
        // ---- delta_tokens --------------------------------------------------

        /// `delta_tokens` saturates: for any usage, the result is
        /// `<= u64::MAX`. The whole function is `saturating_sub` +
        /// `saturating_add`, so it can't overflow — this is a
        /// regression guard, not a bug probe.
        #[test]
        fn prop_delta_saturates(u in arb_usage()) {
            let d = Goal::delta_tokens(&u);
            prop_assert!(d <= u64::MAX);
        }

        /// Cache-read is excluded: increasing `cache_read_tokens`
        /// with `prompt_tokens` fixed never increases the delta.
        #[test]
        fn prop_delta_excludes_cache_read(
            prompt in 0usize..=100_000usize,
            cr1 in 0u64..=100_000u64,
            cr2 in 0u64..=100_000u64,
            completion in 0usize..=100_000usize,
        ) {
            let base = usage(prompt, Some(0), completion);
            let a = usage(prompt, Some(cr1), completion);
            let b = usage(prompt, Some(cr2), completion);
            let base_delta = Goal::delta_tokens(&base);
            prop_assert!(Goal::delta_tokens(&a) <= base_delta);
            prop_assert!(Goal::delta_tokens(&b) <= base_delta);
            // More cache-read -> smaller or equal delta.
            let (small, large) = if cr1 <= cr2 { (cr1, cr2) } else { (cr2, cr1) };
            prop_assert!(
                Goal::delta_tokens(&usage(prompt, Some(large), completion))
                    <= Goal::delta_tokens(&usage(prompt, Some(small), completion)),
            );
        }

        /// No cache state reported (`None`) means cache-read is
        /// treated as 0: the delta equals the `Some(0)` delta.
        #[test]
        fn prop_delta_none_equals_some_zero(u in arb_usage()) {
            let mut no_cache = u.clone();
            no_cache.cache_read_tokens = None;
            let mut zero_cache = u.clone();
            zero_cache.cache_read_tokens = Some(0);
            prop_assert_eq!(
                Goal::delta_tokens(&no_cache),
                Goal::delta_tokens(&zero_cache),
            );
        }

        // ---- observe_usage / observe_time: monotone accumulation ----------

        /// `observe_usage` is monotone: any sequence of observations
        /// leaves `tokens_used` non-decreasing.
        #[test]
        fn prop_observe_usage_monotone(seq in arb_usage_seq()) {
            let mut g = Goal::new(1, "goal");
            let mut prev = g.tokens_used;
            for u in &seq {
                g.observe_usage(u);
                prop_assert!(
                    g.tokens_used >= prev,
                    "tokens_used decreased from {} to {}",
                    prev, g.tokens_used,
                );
                prev = g.tokens_used;
            }
        }

        /// `observe_time` is monotone.
        #[test]
        fn prop_observe_time_monotone(seq in prop::collection::vec(0u64..=10_000u64, 0..=6)) {
            let mut g = Goal::new(1, "goal");
            let mut prev = g.time_used_seconds;
            for s in &seq {
                g.observe_time(*s);
                prop_assert!(
                    g.time_used_seconds >= prev,
                    "time_used_seconds decreased from {} to {}",
                    prev, g.time_used_seconds,
                );
                prev = g.time_used_seconds;
            }
        }

        // ---- terminal status is a hard stop -------------------------------

        /// Once `status` is terminal (`Complete` or `Dropped`), no
        /// observation changes the counters or the status.
        #[test]
        fn prop_terminal_blocks_observation(
            terminal in any_terminal(),
            seq in arb_usage_seq(),
        ) {
            let mut g = Goal::new(1, "goal");
            g.observe_usage(&usage(100, None, 50));
            let tokens_before = g.tokens_used;
            let time_before = g.time_used_seconds;
            g.status = terminal;
            for u in &seq {
                g.observe_usage(u);
                prop_assert_eq!(g.tokens_used, tokens_before);
            }
            g.observe_time(9_999_999);
            prop_assert_eq!(g.time_used_seconds, time_before);
            prop_assert!(g.status.is_terminal());
        }

        // ---- budget-limited is a one-way door under observations ----------

        /// Once `BudgetLimited`, further observations do not return
        /// it to `Active`. The only way out is a caller action
        /// (raising the budget — not in this module's scope).
        #[test]
        fn prop_budget_limited_is_sticky(
            budget in 1u64..=10_000u64,
            seq in arb_usage_seq(),
        ) {
            let mut g = Goal::new(1, "goal").with_token_budget(budget);
            // Force into BudgetLimited by observing well over the cap.
            g.observe_usage(&usage((budget as usize) * 2, None, 0));
            if g.status != GoalStatus::BudgetLimited {
                return Ok(());
            }
            for u in &seq {
                g.observe_usage(u);
                prop_assert_eq!(
                    g.status,
                    GoalStatus::BudgetLimited,
                    "observation moved goal out of BudgetLimited",
                );
            }
        }

        // ---- take_budget_steer: fires exactly once -----------------------

        /// On a `BudgetLimited` goal, `take_budget_steer` returns
        /// true on the first call and false forever after.
        #[test]
        fn prop_steer_fires_exactly_once(
            budget in 1u64..=10_000u64,
            attempts in 1usize..=10usize,
        ) {
            let mut g = Goal::new(1, "goal").with_token_budget(budget);
            // Push to BudgetLimited.
            g.observe_usage(&usage((budget as usize) * 2, None, 0));
            if g.status != GoalStatus::BudgetLimited {
                return Ok(());
            }
            prop_assert!(
                g.take_budget_steer(),
                "steer did not fire on a BudgetLimited goal",
            );
            for _ in 1..attempts {
                prop_assert!(
                    !g.take_budget_steer(),
                    "steer fired more than once",
                );
            }
        }

        // ---- wants_continuation: exact equivalence -----------------------

        /// `wants_continuation` is exactly `status == Active`. No
        /// other status (BudgetLimited, Paused, Complete, Dropped)
        /// returns true.
        #[test]
        fn prop_wants_continuation_iff_active(status in prop::sample::select(vec![
            GoalStatus::Active,
            GoalStatus::BudgetLimited,
            GoalStatus::Paused,
            GoalStatus::Complete,
            GoalStatus::Dropped,
        ])) {
            let mut g = Goal::new(1, "goal");
            g.status = status;
            prop_assert_eq!(
                g.wants_continuation(),
                status == GoalStatus::Active,
                "status {:?}", status,
            );
        }

        /// `is_terminal` is exactly `Complete | Dropped`. No other
        /// status is terminal; both terminal ones are.
        #[test]
        fn prop_is_terminal_exact(status in prop::sample::select(vec![
            GoalStatus::Active,
            GoalStatus::BudgetLimited,
            GoalStatus::Paused,
            GoalStatus::Complete,
            GoalStatus::Dropped,
        ])) {
            let expected = matches!(status, GoalStatus::Complete | GoalStatus::Dropped);
            prop_assert_eq!(status.is_terminal(), expected, "status {:?}", status);
        }
    }

    /// Control: a small token budget is reachable to `BudgetLimited`
    /// via `observe_usage`, so the sticky and once-steer properties
    /// are not vacuous.
    #[test]
    fn control_budget_limit_reachable() {
        let mut g = Goal::new(1, "goal").with_token_budget(100);
        g.observe_usage(&usage(1_000, None, 0));
        assert_eq!(g.status, GoalStatus::BudgetLimited);
    }

    /// Control: a zero-token delta (empty usage) is a no-op — the
    /// guard against "observe with all-zero usage flips to limited
    /// on a zero budget" (which would be a subtle bug: a
    /// `with_token_budget(0)` goal would be limited from the start,
    /// but a single zero observation should not change that).
    #[test]
    fn control_zero_usage_is_noop_for_token_counters() {
        let mut g = Goal::new(1, "goal");
        let before = g.tokens_used;
        g.observe_usage(&usage(0, None, 0));
        assert_eq!(g.tokens_used, before);
    }
}
