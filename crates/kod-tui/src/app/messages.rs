//! Message-list state on `KodApp`.
//!
//! Extracted from `app/mod.rs` (S9). Covers the transcript itself —
//! adding, clearing (with undo), pinning, editing, forking, and the
//! scroll position that keeps the newest turn visible.

use super::*;

impl KodApp {
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub fn add_message(&mut self, message: Message) {
        // Sticky bottom: pin to live only if the user was already there.
        // Otherwise a new arrival mid-read would yank the viewport away.
        let pinned = self.scroll_lines == 0;
        let mut message = message;
        message.sequence = self.next_seq;
        self.next_seq += 1;
        self.messages.push(message);
        if pinned {
            self.scroll_to_bottom();
        }
    }

    pub fn clear_messages(&mut self) {
        if !self.messages.is_empty() {
            self.cleared_stack.push(std::mem::take(&mut self.messages));
            if self.cleared_stack.len() > 5 {
                self.cleared_stack.remove(0);
            }
        }
        self.scroll_lines = 0;
        self.expanded_tools.clear();
        self.clear_search();

        // Reset context accounting. `/clear` wipes the display AND the
        // engine's transcript (see the ConfirmKind::Clear handler in
        // main_loop, which calls engine.clear_history()), so the
        // session really is starting over. Leaving the previous
        // session's accumulated `context_tokens` in place meant the
        // next N messages inherited a count that included messages
        // the user had thrown away: the header's "≈ ctx X/Y" meter
        // overstated by the discarded amount, and `maybe_compact`'s
        // threshold — a fraction of the model window — was compared
        // against a number that no longer reflected anything.
        //
        // `compacted_messages` (the session's running total) is reset
        // for the same reason: it is meant to say "N messages have
        // been compacted *in this session*", not "since the process
        // started".
        self.context_tokens = 0;
        self.compacted_messages = 0;
        // Session cost accumulates per session, and `/clear` starts
        // a new session: the previous spend is no longer this
        // session's.
        self.session_cost_usd = 0.0;
    }

    /// Restore the most recently cleared chat. False when nothing is stashed.
    pub fn undo_clear(&mut self) -> bool {
        match self.cleared_stack.pop() {
            Some(msgs) => {
                self.messages = msgs;
                self.scroll_to_bottom();
                true
            }
            None => false,
        }
    }

    pub fn has_undo(&self) -> bool {
        !self.cleared_stack.is_empty()
    }

    /// Set the pinned flag on the message at `idx` (0-based index into
    /// the full message list, so tool and system rows count). Returns
    /// `true` when the index was in range.
    ///
    /// The engine keeps its own copy of the pin state (a turn's
    /// `metadata.pinned`); the TUI mirrors it here so the display
    /// stays in sync without a round-trip. The two can drift if the
    /// engine compacts a turn the TUI still shows; the pin marker is
    /// cosmetic in that case — the model still gets the pinned text.
    pub fn set_message_pinned_at(&mut self, idx: usize, pinned: bool) -> bool {
        match self.messages.get_mut(idx) {
            Some(m) => {
                m.metadata.pinned = pinned;
                true
            }
            None => false,
        }
    }

    /// True when the message at `idx` is pinned.
    pub fn is_message_pinned(&self, idx: usize) -> bool {
        self.messages
            .get(idx)
            .map(|m| m.metadata.pinned)
            .unwrap_or(false)
    }

    /// Load your last user message back into the input box for editing.
    /// Returns false when there is nothing to edit.
    pub fn edit_last_message(&mut self) -> bool {
        let last = self
            .messages
            .iter()
            .rev()
            .find(|m| m.role == MessageRole::User)
            .map(|m| m.content.clone());
        match last {
            Some(text) => {
                self.set_input(text);
                self.reset_history_index();
                true
            }
            None => false,
        }
    }

    pub fn push_assistant_message(&mut self, content: &str) {
        // LLMs (and streamed chunks) often start with blank lines — even
        // whitespace-only ones (`"\n  \nHello"`). A raw push renders those
        // as an empty first row inside the bubble. Collapse leading and
        // trailing blank lines, keep interior formatting intact.
        let trimmed = Self::trim_blank_lines(content);
        if trimmed.is_empty() {
            return;
        }
        self.add_message(Message {
            id: MessageId::new(),
            role: MessageRole::Assistant,
            content: trimmed.to_string(),
            timestamp: Utc::now(),
            metadata: MessageMetadata::default(),
            sequence: 0,
        });
    }

    pub fn push_system_message(&mut self, content: &str) {
        self.add_message(Message {
            id: MessageId::new(),
            role: MessageRole::System,
            content: content.to_string(),
            timestamp: Utc::now(),
            metadata: MessageMetadata::default(),
            sequence: 0,
        });
    }

    /// Strip leading/trailing blank lines (whitespace-only counts as blank),
    /// preserving interior formatting.
    pub(crate) fn trim_blank_lines(s: &str) -> String {
        let lines: Vec<&str> = s.lines().collect();
        let mut start = 0;
        while start < lines.len() && lines[start].trim().is_empty() {
            start += 1;
        }
        let mut end = lines.len();
        while end > start && lines[end - 1].trim().is_empty() {
            end -= 1;
        }
        lines[start..end].join("\n")
    }

    /// Remove the trailing (assistant, ...) pair through the preceding
    /// user message. Returns the removed user text when something was
    /// removed, so the caller can feed it back into a new turn.
    pub fn drop_last_exchange(&mut self) -> Option<String> {
        // Walk back from the end: drop tool / assistant / system
        // messages, then the user message, then stop.
        let mut user_text: Option<String> = None;
        while let Some(msg) = self.messages.last() {
            match msg.role {
                kod_types::MessageRole::User => {
                    user_text = Some(msg.content.clone());
                    self.messages.pop();
                    break;
                }
                kod_types::MessageRole::Assistant
                | kod_types::MessageRole::Tool
                | kod_types::MessageRole::System => {
                    self.messages.pop();
                }
                kod_types::MessageRole::Agent(_) => {
                    self.messages.pop();
                }
            }
        }
        user_text
    }

    /// Replace the current chat with `messages`, resetting `sequence`
    /// so the widget's sort is a no-op. Refuses to add a `sequence`
    /// value past the existing `next_seq`.
    pub fn replace_messages(&mut self, mut messages: Vec<Message>) {
        for (i, m) in messages.iter_mut().enumerate() {
            m.sequence = i as u64;
        }
        self.next_seq = messages.len() as u64;
        self.messages = messages;
        self.scroll_to_bottom();
    }

    /// Save the current chat as a restorable fork. The current chat is
    /// left in place; `/undo` will later restore this saved copy. A
    /// subsequent `/clear` drops the live chat and leaves the fork on
    /// `cleared_stack`, giving the user a way back.
    pub fn fork_messages(&mut self) -> usize {
        if self.messages.is_empty() {
            return 0;
        }
        let snapshot = self.messages.clone();
        let n = snapshot.len();
        self.cleared_stack.push(snapshot);
        // Keep the stack bounded.
        while self.cleared_stack.len() > 5 {
            self.cleared_stack.remove(0);
        }
        n
    }

    /// Number of saved forks available via /undo.
    pub fn fork_count(&self) -> usize {
        self.cleared_stack.len()
    }

    /// Scroll back (look at older lines)
    pub fn scroll_up(&mut self, lines: usize) {
        self.scroll_lines = self.scroll_lines.saturating_add(lines);
    }

    /// Scroll forward (toward live bottom)
    pub fn scroll_down(&mut self, lines: usize) {
        self.scroll_lines = self.scroll_lines.saturating_sub(lines);
    }

    pub fn scroll_to_bottom(&mut self) {
        self.scroll_lines = 0;
    }

    /// Jump to the oldest content (render clamps to the real maximum).
    pub fn scroll_to_top(&mut self) {
        self.scroll_lines = usize::MAX;
    }

    pub fn is_scrolled_to_bottom(&self) -> bool {
        self.scroll_lines == 0
    }

    pub fn scroll_offset(&self) -> usize {
        self.scroll_lines
    }
}
