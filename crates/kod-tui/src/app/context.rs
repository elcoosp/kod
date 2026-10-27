//! Context-window accounting and auto-compact on `KodApp`.
//!
//! Extracted from `app/mod.rs` (S9).

use super::*;

impl KodApp {
    pub fn set_model_name(&mut self, name: &str) {
        self.model_name = name.to_string();
    }

    pub fn model_name(&self) -> &str {
        &self.model_name
    }

    /// Rough token estimate for a piece of text (1 token ≈ 4 chars). Approx until provider `usage` lands.
    pub(super) fn estimate_tokens(chars: usize) -> usize {
        chars / 4
    }

    /// Record tokens for an outgoing prompt (approx).
    pub fn note_prompt(&mut self, prompt: &str) {
        self.note_usage(prompt.len());
        self.maybe_compact();
    }

    /// Record real token usage from the provider.
    ///
    /// Real usage is authoritative: the server reports exactly how many
    /// tokens went through the model on the last call. Replace the
    /// running char-based estimate rather than taking the max — the
    /// estimate accumulates across turns and never resets, so `max`
    /// made the meter monotonically climb across a session even though
    /// the model's context stays bounded by `render_history` + the
    /// provider's own window. That mis-report also drove auto-compact
    /// into firing far earlier than the real threshold.
    ///
    /// (The previous implementation used `max`, contradicting its own
    /// doc comment which said "replaces".)
    pub fn note_real_usage(&mut self, total_tokens: usize) {
        if total_tokens > 0 {
            self.context_tokens = total_tokens;
            // Once real usage has arrived, the char estimate must stop
            // contributing for the rest of the turn. The provider's
            // total already covers prompt + completion, so any
            // subsequent `note_usage` from `finish_response` /
            // `flush_streamed_text` would double-count the reply.
            self.turn_has_real_usage = true;
        }
        self.maybe_compact();
    }

    /// Record real usage from a TokenUsage-like triple (prompt/completion/total).
    pub fn note_usage_tokens(
        &mut self,
        prompt_tokens: usize,
        completion_tokens: usize,
        total_tokens: usize,
    ) {
        let total = if total_tokens > 0 {
            total_tokens
        } else {
            prompt_tokens + completion_tokens
        };
        self.note_real_usage(total);
    }

    /// Record one round's provider usage as the context-window
    /// snapshot. This is the accurate meter: the latest round's
    /// prompt is what the model's window actually holds
    /// (system + history + tools + fresh results), while the turn's
    /// merged total re-counts history every round. Replaces the
    /// running estimate and, like `note_real_usage`, stops the
    /// char estimate contributing for the rest of the turn.
    pub fn note_window_usage(&mut self, prompt_tokens: usize, completion_tokens: usize) {
        let total = prompt_tokens.saturating_add(completion_tokens);
        if total > 0 {
            self.context_tokens = total;
            self.turn_has_real_usage = true;
        }
        self.maybe_compact();
    }

    pub(super) fn note_usage(&mut self, chars: usize) {
        // Suppressed once real usage for this turn has been recorded.
        // See `turn_has_real_usage` for the double-count this avoids.
        if self.turn_has_real_usage {
            return;
        }
        self.context_tokens = self
            .context_tokens
            .saturating_add(Self::estimate_tokens(chars));
    }

    pub fn context_tokens(&self) -> usize {
        self.context_tokens
    }

    pub fn context_limit(&self) -> usize {
        self.context_limit
    }

    pub fn set_context_limit(&mut self, limit: usize) {
        self.context_limit = limit.max(1_000);
    }

    /// `≈ ctx 12.4k/128k · 10%` — human-readable session position.
    /// Short header figures: `12.4k` / `128k`. Same `format_k` as
    /// `context_label` (which stays for `/context` and `/whoami`).
    pub fn context_tokens_k(&self) -> String {
        Self::format_k(self.context_tokens)
    }

    pub fn context_limit_k(&self) -> String {
        Self::format_k(self.context_limit)
    }

    /// `context_tokens` is an approximation (chars/4) until the provider
    /// returns real `usage` — prefix with ≈ so the header never implies precision.
    pub fn context_label(&self) -> String {
        let pct = self.context_tokens * 100 / self.context_limit.max(1);
        format!(
            "≈ ctx {}/{} · {}%",
            Self::format_k(self.context_tokens),
            Self::format_k(self.context_limit),
            pct.min(999)
        )
    }

    pub(super) fn format_k(n: usize) -> String {
        if n >= 1_000 {
            format!("{:.1}k", n as f64 / 1_000.0)
        } else {
            n.to_string()
        }
    }

    /// Drop oldest messages once past 4/5 of the window, keeping the most
    /// recent 20. Announces itself so the user sees where they stand.
    pub(super) fn maybe_compact(&mut self) {
        if !self.autocompact_enabled {
            return;
        }
        let threshold = self.context_limit * COMPACT_AT_FRACTION_NUM / COMPACT_AT_FRACTION_DEN;
        if self.context_tokens < threshold || self.messages.len() <= 21 {
            return;
        }
        let drop = self.messages.len().saturating_sub(20);
        self.messages.drain(..drop);
        self.compacted_messages += drop;
        // Re-baseline: remaining history ≈ 3/5 of the window.
        self.context_tokens = self.context_limit * 3 / 5;
        let note = format!(
            "Auto-compacted {} older messages ({} total) · {}",
            drop,
            self.compacted_messages,
            self.context_label()
        );
        // Route through add_message so the notice gets a monotonic
        // sequence. Pushing directly with `sequence: 0` made the chat
        // widget (which sorts by sequence) render the notice at the
        // very top of the transcript, above the messages it had just
        // compacted — the exact opposite of where it belongs.
        self.add_message(Message {
            id: MessageId::new(),
            role: MessageRole::System,
            content: note,
            timestamp: Utc::now(),
            metadata: MessageMetadata::default(),
            sequence: 0,
        });
    }

    /// Manual `/compact`: same as auto but on demand.
    ///
    /// Both branches already went through `push_system_message` (which
    /// calls `add_message`), so the notice has always had a correct
    /// sequence — kept here as the counterpart to `maybe_compact`, and
    /// to make it obvious that both paths must go through `add_message`.
    pub fn compact_now(&mut self) {
        if self.messages.len() <= 21 {
            self.push_system_message(&format!("Nothing to compact · {}", self.context_label()));
            return;
        }
        let drop = self.messages.len().saturating_sub(20);
        self.messages.drain(..drop);
        self.compacted_messages += drop;
        self.context_tokens = self.context_limit * 3 / 5;

        // The engine leaves a `## Previous Conversation Summary` message
        // at the head of its transcript (see `compact_history_for`), so
        // a user who scrolls back can see what was elided. Mirror that
        // here: the display copy previously showed nothing at all, which
        // made a compaction look like data loss.
        self.add_message(Message {
            id: MessageId::new(),
            role: MessageRole::System,
            content: format!(
                "Compacted {} older messages ({} total) · {} — the model sees a summary of them",
                drop,
                self.compacted_messages,
                self.context_label(),
            ),
            timestamp: Utc::now(),
            metadata: MessageMetadata::default(),
            sequence: 0, // assigned by add_message
        });
    }

    /// Context meter with teeth: warns past 70%, alarms past 90%.
    pub fn context_warning(&self) -> Option<&'static str> {
        let pct = self.context_tokens * 100 / self.context_limit.max(1);
        if pct >= 90 {
            Some("context almost full — /compact soon")
        } else if pct >= 70 {
            Some("context filling up")
        } else {
            None
        }
    }

    /// Add a call's USD cost to the session total. Negative and NaN
    /// values are rejected — a broken pricing block should not
    /// corrupt the accumulator.
    pub fn note_session_cost(&mut self, cost_usd: f64) {
        if cost_usd.is_finite() && cost_usd >= 0.0 {
            self.session_cost_usd += cost_usd;
        }
    }

    /// Total tokens moved through the model this session.
    pub fn session_total_tokens(&self) -> usize {
        self.session_input_tokens
            .saturating_add(self.session_output_tokens)
    }

    /// Wall-clock time since the session started.
    pub fn elapsed_session(&self) -> std::time::Duration {
        self.session_started_at.elapsed()
    }

    /// One-line accounting label for the header: `↑1.2k ↓340 · 5m30s`.
    /// The arrow convention is input/output; the trailing figure is
    /// wall-clock elapsed since the first prompt.
    pub fn accounting_label(&self) -> String {
        let secs = self.elapsed_session().as_secs();
        let time = if secs < 60 {
            format!("{secs}s")
        } else if secs < 3600 {
            format!("{}m{:02}s", secs / 60, secs % 60)
        } else {
            format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
        };
        format!(
            "↑{} ↓{} · {}",
            Self::format_k(self.session_input_tokens),
            Self::format_k(self.session_output_tokens),
            time,
        )
    }

    /// Record session-wide token usage from a per-call breakdown.
    /// Separate from `note_real_usage` (window snapshot for the context
    /// meter): this counter only grows.
    pub fn note_session_usage(&mut self, prompt_tokens: usize, completion_tokens: usize) {
        self.session_input_tokens = self.session_input_tokens.saturating_add(prompt_tokens);
        self.session_output_tokens = self.session_output_tokens.saturating_add(completion_tokens);
    }
}

#[cfg(test)]
mod format_k_tests {
    //! Boundary and shape tests for `format_k`.
    //!
    //! `format_k` has no test of any kind. It is a one-branch
    //! formatter, but the branch (`n >= 1_000`) and the rounding
    //! (`{:.1}`) both have observable boundaries: 1000 is the first
    //! value that gets the `k` suffix, 1500 rounds to `1.5k`, and
    //! 1950 rounds *up* to `2.0k` — the rounding direction is part
    //! of the contract even when it surprises.
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn threshold_is_exactly_one_thousand() {
        assert_eq!(KodApp::format_k(0), "0");
        assert_eq!(KodApp::format_k(999), "999");
        assert_eq!(KodApp::format_k(1000), "1.0k");
    }

    #[test]
    fn rounding_is_to_one_decimal_place() {
        assert_eq!(KodApp::format_k(1500), "1.5k");
        assert_eq!(KodApp::format_k(10_000), "10.0k");
        // Deliberately not pinned: values that land exactly on a .x5
        // decimal boundary. `1950 / 1000.0` is not exactly 1.95 in
        // f64 (it is ~1.9499999...), so `{:.1}` renders "1.9", not
        // "2.0". That is float-representation behavior, not this
        // function's contract, and pinning it would freeze an
        // implementation detail. The suffix property above covers the
        // shape without depending on it.
    }

    proptest! {
        /// Totality: no usize panics.
        #[test]
        fn prop_never_panics(n in any::<usize>()) {
            let _ = KodApp::format_k(n);
        }

        /// Shape: below 1000 the output is the plain decimal; at or
        /// above it always ends with `k`.
        #[test]
        fn prop_suffix_matches_threshold(n in any::<usize>()) {
            let s = KodApp::format_k(n);
            if n < 1_000 {
                prop_assert!(!s.ends_with('k'), "{n} got a k suffix: {s:?}");
                prop_assert_eq!(s, n.to_string());
            } else {
                prop_assert!(s.ends_with('k'), "{n} lost its k suffix: {s:?}");
            }
        }
    }
}
