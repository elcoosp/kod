//! Provider-anchored context token accounting (borrow from oh-my-pi,
//! delta §2.4).
//!
//! # The problem
//!
//! "How large is my context?" is answered today by walking the
//! transcript and multiplying character counts by a fixed ratio. That
//! is O(transcript) work per turn, and the ratio is a guess — the same
//! bytes tokenize to a different count for different models, and
//! different content (prose vs code vs JSON) tokenizes at different
//! densities. The compaction threshold and the `/debug tokens` readout
//! both ride on that guess.
//!
//! # The observation
//!
//! After every completed provider call, the provider tells us exactly
//! how many input tokens it processed. That number is the ground truth
//! for every byte we sent. It is not a guess and it is not a
//! per-model approximation — it is what the provider actually billed.
//!
//! # The trick
//!
//! Anchor on the last settled usage report. Everything up to and
//! including the message at that index is represented by the anchor's
//! `context_tokens`. Only the *tail* — messages appended after the
//! anchor — needs a local estimate. That turns per-turn accounting
//! from O(transcript) into O(new messages since the last call), which
//! is O(1) in the common case of a two-message append.
//!
//! # What this does NOT do
//!
//! It does not detect a *changed* prefix. If a caller mutates an
//! already-anchored message in place (compaction, a system-prompt
//! edit, a model switch), the anchor is now lying about the prefix
//! the provider is charging for. The caller must call [`clear`] on
//! those events — the same events P0's `CacheLedger` invalidation
//! already tracks. Wiring the two together is a follow-up; this module
//! stays a pure primitive so each caller can decide when an anchor is
//! no longer valid.
//!
//! # Relationship to `TokenUsage`
//!
//! kod's [`TokenUsage::prompt_tokens`] is documented as the *total*
//! input window the provider processed — the provider crates fold
//! Anthropic's separate `cache_read_input_tokens` /
//! `cache_creation_input_tokens` into it at the boundary. The anchor
//! therefore reads `prompt_tokens` alone. Adding the cache fields
//! would double-count. (This diverges from the design note's
//! `prompt + cache_read + cache_creation` formula, which was written
//! against omp's wire convention where the three are disjoint.)

use kod_provider::TokenUsage;

/// Per-transcript anchor on the provider's own last settled usage.
///
/// Cheap to clone. One per transcript key; the engine holds one per
/// swarm agent plus one for the interactive session.
#[derive(Debug, Clone, Default)]
pub struct ContextGauge {
    anchor: Option<Anchor>,
}

/// What the anchor remembers about one settled call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Anchor {
    /// Index, into the message vector that was *sent*, of the last
    /// message the provider's usage report covers. All messages with
    /// index `<= covers_through` are represented by `context_tokens`.
    covers_through: usize,
    /// The provider's `prompt_tokens` on that call. This is the full
    /// input window the provider billed — system prompt, tool
    /// schemas, every message up to `covers_through` inclusive.
    context_tokens: u64,
    /// Wall-clock milliseconds at observation, for a UI readout that
    /// wants to show the age of the anchor. Not used for decisions.
    observed_at_ms: u64,
}

impl ContextGauge {
    /// A gauge with no anchor yet. The first turn of a session lives
    /// here until the first provider call completes.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a settled usage report.
    ///
    /// `covers_through` is the index, in the message vector that was
    /// sent, of the last message the report covers. In the common
    /// case it is `messages.len() - 1` — the whole transcript was
    /// sent. A caller that sent a truncated transcript passes the
    /// index of the last message it actually sent.
    ///
    /// The observation is ignored when `usage.prompt_tokens == 0` — a
    /// provider that reported no input tokens told us nothing about
    /// the prompt size, and anchoring on zero would be worse than
    /// having no anchor. This is the "settle gate": the engine should
    /// also decline to call this for turns whose stop reason was
    /// `aborted` or `error`, but that policy lives at the call site
    /// because `TokenUsage` does not carry a stop reason.
    pub fn observe(&mut self, covers_through: usize, usage: &TokenUsage) {
        if usage.prompt_tokens == 0 {
            return;
        }
        self.anchor = Some(Anchor {
            covers_through,
            context_tokens: usage.prompt_tokens as u64,
            observed_at_ms: now_ms(),
        });
    }

    /// Forget the anchor.
    ///
    /// Call this on any event that invalidates the cached prefix the
    /// provider last charged for: a compaction (messages removed), a
    /// model switch, a tool-surface change, a system-prompt edit.
    /// After `clear`, [`estimate`] returns `None` until the next
    /// settled call re-anchors.
    pub fn clear(&mut self) {
        self.anchor = None;
    }

    /// The anchor, if one exists: `(covers_through, context_tokens)`.
    /// For tests and for a UI readout that wants to show what the
    /// gauge is standing on.
    pub fn anchor(&self) -> Option<(usize, u64)> {
        self.anchor.map(|a| (a.covers_through, a.context_tokens))
    }

    /// True when a settled usage report has been observed and not yet
    /// cleared. A caller about to walk the transcript can check this
    /// and skip the walk entirely.
    pub fn is_anchored(&self) -> bool {
        self.anchor.is_some()
    }

    /// The first message index NOT covered by the anchor.
    ///
    /// `Some(n)` means messages at indices `0..n` are already paid for
    /// by the anchor and only `messages[n..]` needs a local estimate.
    /// `None` when there is no anchor — the caller estimates the whole
    /// transcript.
    pub fn tail_start(&self) -> Option<usize> {
        self.anchor.map(|a| a.covers_through + 1)
    }

    /// The anchored total plus a caller-supplied tail estimate.
    ///
    /// Returns `None` when there is no anchor, so a caller that wants
    /// a definite number can fall back to its own char-arithmetic
    /// without this module inventing a placeholder.
    pub fn estimate(&self, tail_tokens: u64) -> Option<u64> {
        self.anchor
            .map(|a| a.context_tokens.saturating_add(tail_tokens))
    }

    /// [`estimate`] with a caller-supplied fallback for the
    /// unanchored case. Convenience for the common "use the anchor if
    /// we have one, otherwise use the char-arithmetic estimate" shape.
    pub fn estimate_or(&self, tail_tokens: u64, fallback: u64) -> u64 {
        self.estimate(tail_tokens).unwrap_or(fallback)
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(prompt: usize) -> TokenUsage {
        TokenUsage {
            prompt_tokens: prompt,
            completion_tokens: 10,
            total_tokens: prompt + 10,
            cache_read_tokens: None,
            cache_creation_tokens: None,
        }
    }

    #[test]
    fn a_fresh_gauge_is_unanchored() {
        let g = ContextGauge::new();
        assert!(!g.is_anchored());
        assert!(g.anchor().is_none());
        assert!(g.tail_start().is_none());
        assert!(g.estimate(123).is_none());
    }

    #[test]
    fn observe_anchors_on_the_prompt_tokens() {
        let mut g = ContextGauge::new();
        g.observe(4, &usage(1500));
        assert!(g.is_anchored());
        assert_eq!(g.anchor(), Some((4, 1500)));
    }

    #[test]
    fn estimate_is_anchor_plus_tail() {
        let mut g = ContextGauge::new();
        g.observe(4, &usage(1500));
        assert_eq!(g.estimate(200), Some(1700));
    }

    #[test]
    fn tail_start_is_the_index_after_the_anchor() {
        let mut g = ContextGauge::new();
        g.observe(4, &usage(1500));
        assert_eq!(g.tail_start(), Some(5));
    }

    #[test]
    fn zero_prompt_tokens_do_not_anchor() {
        // A provider that reported no input tokens told us nothing.
        // Anchoring on zero would make `estimate` lie downward and
        // push the compaction trigger past the model's real limit.
        let mut g = ContextGauge::new();
        g.observe(7, &usage(0));
        assert!(!g.is_anchored());
    }

    #[test]
    fn a_second_observe_replaces_the_first() {
        let mut g = ContextGauge::new();
        g.observe(2, &usage(500));
        g.observe(9, &usage(2000));
        assert_eq!(g.anchor(), Some((9, 2000)));
        assert_eq!(g.estimate(100), Some(2100));
    }

    #[test]
    fn clear_removes_the_anchor() {
        let mut g = ContextGauge::new();
        g.observe(4, &usage(1500));
        g.clear();
        assert!(!g.is_anchored());
        assert!(g.estimate(999).is_none());
    }

    #[test]
    fn estimate_or_uses_the_fallback_when_unanchored() {
        let g = ContextGauge::new();
        assert_eq!(g.estimate_or(200, 1200), 1200);
    }

    #[test]
    fn estimate_or_uses_the_anchor_when_present() {
        let mut g = ContextGauge::new();
        g.observe(4, &usage(1500));
        // Fallback is ignored when the gauge has a real number.
        assert_eq!(g.estimate_or(200, 9999), 1700);
    }

    #[test]
    fn saturating_add_does_not_overflow() {
        // Pathological inputs must saturate, not wrap. A caller that
        // passes `u64::MAX` for a tail cannot make the readout wrap
        // to a tiny number and cause compaction to skip.
        let mut g = ContextGauge::new();
        g.observe(0, &usage(1000));
        assert_eq!(g.estimate(u64::MAX), Some(u64::MAX));
    }

    #[test]
    fn cache_read_tokens_are_not_added_to_the_anchor() {
        // kod's `TokenUsage.prompt_tokens` is the total input window;
        // the provider crates fold Anthropic's cache fields into it at
        // the boundary. Adding `cache_read_tokens` here would
        // double-count every cached turn at the full input rate.
        //
        // This test pins the convention against a future "fix" that
        // reaches for the design note's `prompt + cache_read +
        // cache_creation` formula.
        let mut g = ContextGauge::new();
        let u = TokenUsage {
            prompt_tokens: 10_000,
            completion_tokens: 100,
            total_tokens: 10_100,
            cache_read_tokens: Some(8_000),
            cache_creation_tokens: Some(500),
        };
        g.observe(3, &u);
        assert_eq!(g.anchor(), Some((3, 10_000)));
    }
}
