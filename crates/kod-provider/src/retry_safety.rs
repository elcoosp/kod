//! Replay-safe stream retry (borrow from oh-my-pi, delta §9.2).
//!
//! # The question
//!
//! A streaming response dies mid-flight — a transport error, an idle
//! timeout, a `GOAWAY` frame. The caller wants to retry. But a retry
//! is only safe *before the attempt has committed* to anything the
//! user could see, or that the model would have to re-generate.
//!
//! Retrying after the model has already emitted half an answer means:
//!
//! * the user sees the same half-answer twice (once from the failed
//!   attempt, once from the retry); and
//! * the model burns budget re-doing the reasoning that produced it.
//!
//! The safe/unsafe line is what [`AttemptTracker`] decides. It watches
//! the stream and answers one question: *has this attempt committed?*
//!
//! # What commits
//!
//! The rule, from the design note's §9.2:
//!
//! * **A non-empty `Text` chunk commits.** The model has said
//!   something.
//! * **A non-empty `ToolCallDelta.arguments` commits.** The model
//!   has started specifying a tool call's arguments; retrying would
//!   re-generate them.
//! * **Nothing else commits.** `ToolCallStart` alone is a marker — the
//!   model announced a call but has not specified its arguments, and
//!   if the stream dies right there, discarding the marker and
//!   retrying produces a clean attempt. `Usage`, `StopReason`, and
//!   `Done` are metadata about the stream, not output from the model.
//!   An empty `Text` or an empty `ToolCallDelta` is a no-op.
//!
//! # The empty-completion case
//!
//! A separate but related question: what if the stream *completes
//! cleanly* — no error — but the model produced nothing? An empty
//! reply with a near-zero completion-token count is almost always a
//! transient upstream wobble rather than a genuine "I have nothing to
//! say". [`EmptyCompletionRetry`] bounds the recovery: at most two
//! retries, with the delays the note gives (500 ms doubled per
//! attempt).
//!
//! The two cases are independent. A stream can die mid-flight with
//! output already emitted (not retry-safe), or finish cleanly with
//! no output (retry-safe), or any combination. Each has its own
//! tracker.
//!
//! # Scope
//!
//! This module is the *decision*. It does not retry, it does not
//! sleep, it does not touch the network. A caller feeds chunks into
//! an [`AttemptTracker`], checks [`AttemptTracker::is_safe_to_retry`]
//! on failure, and either surfaces the error or re-enters its
//! streaming loop. A completed stream is examined with
//! [`EmptyCompletionRetry::should_retry`], and the caller either
//! returns the empty reply or backs off and tries again.
//!
//! No integration with `kod-core`'s streaming loop yet. The primitive
//! is testable on its own, and the loop's wiring is a separate
//! change that can carry its own design review.

use crate::types::{StreamChunk, TokenUsage};

/// The maximum number of extra attempts a completed-but-empty reply
/// gets. The note's `≤ 2`.
pub const MAX_EMPTY_COMPLETION_RETRIES: u32 = 2;

/// Base backoff for an empty-completion retry, per the note's
/// `500 ms · 2ⁿ` formula. Attempt `n` (1-based) sleeps
/// `base * 2^(n-1)`; callers apply their own jitter on top.
pub const EMPTY_COMPLETION_BASE_DELAY_MS: u64 = 500;

/// Would a retry of an attempt that has seen this chunk lose
/// meaningful work?
///
/// Free function form, for tests and for a caller that wants to
/// classify a chunk without owning a tracker. The rule is in the
/// module doc.
pub fn chunk_commits(chunk: &StreamChunk) -> bool {
    match chunk {
        StreamChunk::Text(t) => !t.is_empty(),
        // The `ToolCallStart` marker alone does not commit: the model
        // announced a call but has not specified its arguments. A
        // stream that dies before any `ToolCallDelta` can be retried
        // cleanly, provided the caller discards the marker it may
        // already have rendered (a "calling read_file..." spinner,
        // for example).
        StreamChunk::ToolCallStart { .. } => false,
        StreamChunk::ToolCallDelta { arguments, .. } => !arguments.is_empty(),
        // `Usage`, `StopReason`, and `Done` describe the stream, not
        // the model's output. None of them commits.
        StreamChunk::Usage(_) => false,
        StreamChunk::StopReason(_) => false,
        StreamChunk::Done => false,
    }
}

/// Watches one streaming attempt and reports whether it has committed
/// to output a retry cannot reproduce.
///
/// Cheap to construct — one bool. Intended to be created at the start
/// of a streaming attempt and dropped when the attempt ends (retried
/// or not).
#[derive(Debug, Clone, Copy, Default)]
pub struct AttemptTracker {
    committed: bool,
}

impl AttemptTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one chunk. Once a committing chunk is seen, the tracker
    /// stays committed for its lifetime — retry safety is monotone
    /// (an attempt that has committed cannot become uncommitted by
    /// later events).
    pub fn observe(&mut self, chunk: &StreamChunk) {
        if !self.committed && chunk_commits(chunk) {
            self.committed = true;
        }
    }

    /// Has the attempt committed? See the module doc.
    pub fn is_committed(&self) -> bool {
        self.committed
    }

    /// The inverse — the question a caller actually asks when a
    /// stream fails. Provided as a named method so the calling code
    /// reads as `if tracker.is_safe_to_retry() { retry() } else
    /// { surface(err) }` rather than the double-negation.
    pub fn is_safe_to_retry(&self) -> bool {
        !self.committed
    }

    /// Reset for the next attempt. Does not need to be called
    /// between attempts if the caller constructs a fresh tracker
    /// each time — that is the intended usage — but offered for a
    /// caller that reuses one.
    pub fn reset(&mut self) {
        self.committed = false;
    }
}

/// Bounds the recovery for a stream that completed cleanly but
/// produced nothing.
///
/// The note's rule: a stop with no visible content and a
/// completion-token count of at most 1 is worth retrying, up to
/// twice.
#[derive(Debug, Clone, Copy, Default)]
pub struct EmptyCompletionRetry {
    attempts: u32,
}

impl EmptyCompletionRetry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Should a completed stream with this content and usage be
    /// retried?
    ///
    /// Three checks, all required:
    ///
    /// 1. The retry budget is not spent (`attempts < MAX`).
    /// 2. The completed content is empty. A reply that says anything
    ///    at all — even a single character — is not an empty
    ///    completion; it is a real reply, however terse.
    /// 3. The provider's own count of completion tokens is at most 1.
    ///    A provider that reports no usage at all (`None`) is treated
    ///    as "unknown", not as "zero", and the retry is allowed —
    ///    rejecting on absent usage would leave a provider that does
    ///    not report it stuck on every empty reply.
    ///
    /// The third check is what distinguishes "the model had nothing
    /// to say" (a deliberate empty reply with, say, 5 tokens of
    /// reasoning behind it) from "the model produced nothing at all"
    /// (a transient upstream failure that manifested as an empty
    /// stream). The note's threshold of 1 accepts the latter without
    /// admitting the former.
    pub fn should_retry(&self, content: &str, usage: Option<&TokenUsage>) -> bool {
        if self.attempts >= MAX_EMPTY_COMPLETION_RETRIES {
            return false;
        }
        if !content.is_empty() {
            return false;
        }
        match usage {
            Some(u) => u.completion_tokens <= 1,
            None => true,
        }
    }

    /// Record that a retry has been issued. The next `should_retry`
    /// sees the incremented count.
    pub fn observe_retry(&mut self) {
        self.attempts = self.attempts.saturating_add(1);
    }

    /// How many retries have been issued so far.
    pub fn attempts(&self) -> u32 {
        self.attempts
    }

    /// The delay before the `attempts`-th retry (1-based), per the
    /// note's `500 ms · 2ⁿ` formula. Attempt 1 → 500 ms, attempt 2 →
    /// 1 000 ms. Callers add their own jitter.
    ///
    /// Returns `0` when the budget is spent, so a caller that
    /// unconditionally sleeps the returned duration does not add a
    /// spurious pause after the last retry.
    pub fn next_delay_ms(&self) -> u64 {
        if self.attempts >= MAX_EMPTY_COMPLETION_RETRIES {
            return 0;
        }
        let shift = self.attempts;
        EMPTY_COMPLETION_BASE_DELAY_MS.saturating_mul(1u64 << shift)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> StreamChunk {
        StreamChunk::Text(s.to_string())
    }

    fn tool_start(name: &str) -> StreamChunk {
        StreamChunk::ToolCallStart {
            index: 0,
            id: None,
            name: name.to_string(),
        }
    }

    fn tool_delta(args: &str) -> StreamChunk {
        StreamChunk::ToolCallDelta {
            index: 0,
            arguments: args.to_string(),
        }
    }

    fn usage(completion_tokens: usize) -> TokenUsage {
        TokenUsage {
            prompt_tokens: 10,
            completion_tokens,
            total_tokens: 10 + completion_tokens,
            cache_read_tokens: None,
            cache_creation_tokens: None,
        }
    }

    // -----------------------------------------------------------------
    // chunk_commits classifier
    // -----------------------------------------------------------------

    #[test]
    fn a_non_empty_text_commits() {
        assert!(chunk_commits(&text("hello")));
    }

    #[test]
    fn an_empty_text_does_not_commit() {
        // An empty delta is a no-op; nothing was said.
        assert!(!chunk_commits(&text("")));
    }

    #[test]
    fn a_tool_call_start_does_not_commit() {
        // The doc: "stream dying before args → discard markers,
        // retry". A start is a marker, not output.
        assert!(!chunk_commits(&tool_start("read_file")));
    }

    #[test]
    fn a_non_empty_tool_call_delta_commits() {
        assert!(chunk_commits(&tool_delta(r#"{"path":"a"}"#)));
    }

    #[test]
    fn an_empty_tool_call_delta_does_not_commit() {
        assert!(!chunk_commits(&tool_delta("")));
    }

    #[test]
    fn usage_does_not_commit() {
        assert!(!chunk_commits(&StreamChunk::Usage(usage(5))));
    }

    #[test]
    fn stop_reason_does_not_commit() {
        assert!(!chunk_commits(&StreamChunk::StopReason("end_turn".into())));
    }

    #[test]
    fn done_does_not_commit() {
        assert!(!chunk_commits(&StreamChunk::Done));
    }

    // -----------------------------------------------------------------
    // AttemptTracker
    // -----------------------------------------------------------------

    #[test]
    fn a_fresh_tracker_is_safe_to_retry() {
        let t = AttemptTracker::new();
        assert!(!t.is_committed());
        assert!(t.is_safe_to_retry());
    }

    #[test]
    fn observing_a_committing_chunk_flips_the_tracker() {
        let mut t = AttemptTracker::new();
        t.observe(&text("first token"));
        assert!(t.is_committed());
        assert!(!t.is_safe_to_retry());
    }

    #[test]
    fn observing_only_non_committing_chunks_leaves_it_safe() {
        let mut t = AttemptTracker::new();
        t.observe(&StreamChunk::Done);
        t.observe(&StreamChunk::StopReason("stop".into()));
        t.observe(&StreamChunk::Usage(usage(100)));
        t.observe(&tool_start("read_file"));
        t.observe(&tool_delta(""));
        assert!(t.is_safe_to_retry());
    }

    #[test]
    fn commitment_is_monotone() {
        // Once committed, later chunks — even non-committing ones —
        // cannot un-commit the attempt. Retry safety is a one-way
        // gate.
        let mut t = AttemptTracker::new();
        t.observe(&text("x"));
        assert!(t.is_committed());
        t.observe(&StreamChunk::Done);
        t.observe(&text(""));
        assert!(t.is_committed(), "commitment must be monotone");
    }

    #[test]
    fn the_doc_specific_case_tool_start_then_die_is_safe() {
        // The scenario from §9.2: a `ToolCallStart` arrives, then the
        // stream dies before any arguments. The caller can retry,
        // discarding the marker it may have rendered.
        let mut t = AttemptTracker::new();
        t.observe(&tool_start("read_file"));
        assert!(t.is_safe_to_retry());
    }

    #[test]
    fn the_doc_specific_case_tool_start_then_args_is_unsafe() {
        // Same scenario, but the stream gets far enough to deliver
        // argument bytes. Now retrying would lose work.
        let mut t = AttemptTracker::new();
        t.observe(&tool_start("read_file"));
        t.observe(&tool_delta(r#"{"pa"#));
        assert!(!t.is_safe_to_retry());
    }

    #[test]
    fn reset_clears_the_commitment() {
        let mut t = AttemptTracker::new();
        t.observe(&text("x"));
        t.reset();
        assert!(t.is_safe_to_retry());
    }

    // -----------------------------------------------------------------
    // EmptyCompletionRetry
    // -----------------------------------------------------------------

    #[test]
    fn a_fresh_retry_has_zero_attempts() {
        let r = EmptyCompletionRetry::new();
        assert_eq!(r.attempts(), 0);
    }

    #[test]
    fn an_empty_completion_with_zero_tokens_should_retry() {
        let r = EmptyCompletionRetry::new();
        assert!(r.should_retry("", Some(&usage(0))));
    }

    #[test]
    fn an_empty_completion_with_one_token_should_retry() {
        // The note's threshold is `<= 1`.
        let r = EmptyCompletionRetry::new();
        assert!(r.should_retry("", Some(&usage(1))));
    }

    #[test]
    fn an_empty_completion_with_many_tokens_should_not_retry() {
        // The model thought about it and decided to say nothing.
        // That is a real answer, not a transient failure.
        let r = EmptyCompletionRetry::new();
        assert!(!r.should_retry("", Some(&usage(50))));
    }

    #[test]
    fn a_non_empty_completion_should_not_retry() {
        // Any content at all is a real reply, regardless of token
        // count. The content check is first.
        let r = EmptyCompletionRetry::new();
        assert!(!r.should_retry("hello", Some(&usage(1))));
    }

    #[test]
    fn missing_usage_allows_a_retry() {
        // A provider that does not report usage is not silently
        // stuck on every empty reply. `None` is "unknown", not
        // "zero".
        let r = EmptyCompletionRetry::new();
        assert!(r.should_retry("", None));
    }

    #[test]
    fn the_retry_budget_is_bounded_at_two() {
        let mut r = EmptyCompletionRetry::new();
        assert!(r.should_retry("", Some(&usage(0))));
        r.observe_retry();
        assert!(r.should_retry("", Some(&usage(0))));
        r.observe_retry();
        assert!(
            !r.should_retry("", Some(&usage(0))),
            "third attempt must be refused",
        );
        assert_eq!(r.attempts(), 2);
    }

    #[test]
    fn observe_retry_saturates() {
        // Defensive: a caller that calls `observe_retry` more times
        // than `should_retry` allows still does not wrap the counter.
        let mut r = EmptyCompletionRetry::new();
        for _ in 0..10 {
            r.observe_retry();
        }
        assert_eq!(r.attempts(), 10);
        assert!(!r.should_retry("", Some(&usage(0))));
    }

    #[test]
    fn the_first_delay_is_500_ms() {
        let r = EmptyCompletionRetry::new();
        assert_eq!(r.next_delay_ms(), 500);
    }

    #[test]
    fn the_second_delay_is_1000_ms() {
        let mut r = EmptyCompletionRetry::new();
        r.observe_retry();
        assert_eq!(r.next_delay_ms(), 1_000);
    }

    #[test]
    fn the_delay_is_zero_when_the_budget_is_spent() {
        // A caller that unconditionally sleeps `next_delay_ms` does
        // not add a spurious pause after the last retry.
        let mut r = EmptyCompletionRetry::new();
        r.observe_retry();
        r.observe_retry();
        assert_eq!(r.next_delay_ms(), 0);
    }

    #[test]
    fn the_two_trackers_are_independent() {
        // A caller can hold both and use one without affecting the
        // other. Sanity check that neither type has hidden shared
        // state.
        let mut tracker = AttemptTracker::new();
        let mut retry = EmptyCompletionRetry::new();
        tracker.observe(&text("hi"));
        retry.observe_retry();
        assert!(tracker.is_committed());
        assert_eq!(retry.attempts(), 1);
    }
}
