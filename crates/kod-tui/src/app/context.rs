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
        self.push_system_message(&format!(
            "Compacted {} older messages ({} total) · {}",
            drop,
            self.compacted_messages,
            self.context_label()
        ));
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
}
