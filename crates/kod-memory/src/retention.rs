//! Incremental retention with a rolling hash chain (borrow from
//! oh-my-pi, delta §12.6).
//!
//! # The problem
//!
//! A memory extractor that runs once at shutdown sees one long
//! transcript. Continuous retention — extracting as the session runs —
//! must answer a harder question: *what is new since the last
//! extraction?* Sending the whole transcript each time re-extracts
//! everything and grows quadratically.
//!
//! # The rolling hash chain
//!
//! A [`RetentionCursor`] remembers a rolling hash of the transcript
//! prefix it has already retained. Each message updates the chain:
//!
//! ```text
//! hash = H(hash ⧵ role ⧵ content ⧵ timestamp)
//! ```
//!
//! At use time the cursor validates the chain against the current
//! transcript prefix. If the prefix still hashes to the recorded
//! value, the tail is new and can be sent incrementally. If it does
//! not — a rewind, a branch, a compaction rewrote the prefix — the
//! cursor resets and the caller sends the whole transcript again.
//!
//! # Why a chain, not a count
//!
//! A message *count* would say "I saw 10 messages". If message 3 was
//! edited in place, the count is still 10 and the incremental send
//! would miss the change. The chain detects the edit because the
//! recorded prefix hash no longer matches.
//!
//! # What this is NOT
//!
//! * Not the extractor. It answers "what is new"; running a model over
//!   the new tail is the caller's job.
//! * Not persistence. A cursor lives in memory; a caller that wants it
//!   durable writes the hash out.

use kod_types::{ChatMessage, MessageRole};

/// The rolling hash of a retained prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrefixHash(u64);

impl PrefixHash {
    /// The chain's seed: a fixed FNV-1a offset basis. A chain that has
    /// retained nothing hashes to this value.
    pub const SEED: PrefixHash = PrefixHash(0xcbf2_9ce4_8422_2325);

    pub fn value(self) -> u64 {
        self.0
    }

    /// Extend the chain with one message.
    fn extend(self, msg: &ChatMessage) -> PrefixHash {
        let mut h = self.0;
        let mut feed = |b: &[u8]| {
            for &byte in b {
                h ^= byte as u64;
                h = h.wrapping_mul(0x0000_0100_0000_01b3);
            }
        };
        feed(role_tag(&msg.role).as_bytes());
        feed(&[0]);
        feed(msg.content.as_bytes());
        feed(&[0]);
        // The timestamp's seconds, so a message edited to a new time
        // changes the hash.
        feed(&msg.timestamp.unix_timestamp().to_le_bytes());
        PrefixHash(h)
    }
}

fn role_tag(r: &MessageRole) -> &'static str {
    match r {
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
        MessageRole::System => "system",
        MessageRole::Tool => "tool",
        MessageRole::Agent(_) => "agent",
    }
}

/// A cursor over a transcript's retained prefix.
#[derive(Debug, Clone)]
pub struct RetentionCursor {
    /// How many messages the cursor has retained.
    retained: usize,
    /// The rolling hash of those messages.
    hash: PrefixHash,
}

impl Default for RetentionCursor {
    fn default() -> Self {
        Self {
            retained: 0,
            hash: PrefixHash::SEED,
        }
    }
}

impl RetentionCursor {
    pub fn new() -> Self {
        Self::default()
    }

    /// How many messages have been retained.
    pub fn retained(&self) -> usize {
        self.retained
    }

    /// The current chain hash.
    pub fn hash(&self) -> PrefixHash {
        self.hash
    }

    /// Advance the cursor to retain `messages`.
    ///
    /// Returns the count of *new* messages the caller should send —
    /// `None` when the recorded prefix no longer matches (a rewind, a
    /// branch, an in-place edit) and the caller must send the whole
    /// transcript again.
    ///
    /// On a mismatch the cursor resets to `messages.len()` and the new
    /// hash; the next call is incremental from there.
    pub fn advance(&mut self, messages: &[ChatMessage]) -> Option<usize> {
        // Recompute the prefix hash for the recorded length and
        // compare.
        if self.retained <= messages.len() {
            let mut prefix = PrefixHash::SEED;
            for m in &messages[..self.retained] {
                prefix = prefix.extend(m);
            }
            if prefix == self.hash {
                // The prefix is unchanged: the tail is new.
                let new = messages.len() - self.retained;
                self.rehash(messages);
                return Some(new);
            }
        }
        // Either the transcript shrank below the retained count, or the
        // prefix was rewritten. Reset.
        self.rehash(messages);
        None
    }

    /// Reset the cursor to cover `messages` and recompute the chain.
    fn rehash(&mut self, messages: &[ChatMessage]) {
        let mut h = PrefixHash::SEED;
        for m in messages {
            h = h.extend(m);
        }
        self.hash = h;
        self.retained = messages.len();
    }

    /// Reset to the empty cursor.
    pub fn clear(&mut self) {
        *self = Self::default();
    }
}

/// The retention cadence: how often a continuous-retention caller
/// should extract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionCadence {
    /// Run an extraction every N new user turns.
    pub every_n_turns: usize,
    /// The minimum new-message count worth an extraction — a guard
    /// against extracting a two-message tail.
    pub min_new_messages: usize,
}

impl Default for RetentionCadence {
    fn default() -> Self {
        Self {
            every_n_turns: 5,
            min_new_messages: 4,
        }
    }
}

impl RetentionCadence {
    /// Whether the new tail since the last extraction is worth
    /// extracting now. Both floors must pass: `every_n_turns` counts
    /// new *user* turns (assistant/tool traffic alone must not drive
    /// extraction), `min_new_messages` guards tiny tails.
    ///
    /// (Fix: `every_n_turns` used to be dead — `is_due` only checked
    /// the message floor, so any tool-using turn fired extraction.)
    pub fn is_due(&self, new_messages: usize, new_user_turns: usize) -> bool {
        new_user_turns >= self.every_n_turns && new_messages >= self.min_new_messages
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kod_types::MessageId;
    use time::OffsetDateTime;

    fn msg(role: MessageRole, content: &str) -> ChatMessage {
        ChatMessage::text(MessageId::new(), role, content, OffsetDateTime::now_utc())
    }

    fn transcript() -> Vec<ChatMessage> {
        vec![
            msg(MessageRole::User, "first"),
            msg(MessageRole::Assistant, "reply one"),
            msg(MessageRole::User, "second"),
        ]
    }

    #[test]
    fn a_fresh_cursor_retains_nothing() {
        let c = RetentionCursor::new();
        assert_eq!(c.retained(), 0);
        assert_eq!(c.hash(), PrefixHash::SEED);
    }

    #[test]
    fn the_first_advance_retains_the_whole_transcript() {
        let mut c = RetentionCursor::new();
        let t = transcript();
        let new = c.advance(&t).unwrap();
        assert_eq!(new, 3);
        assert_eq!(c.retained(), 3);
    }

    #[test]
    fn an_appended_message_is_one_new() {
        let mut c = RetentionCursor::new();
        let mut t = transcript();
        c.advance(&t).unwrap();
        t.push(msg(MessageRole::Assistant, "reply two"));
        let new = c.advance(&t).unwrap();
        assert_eq!(new, 1);
        assert_eq!(c.retained(), 4);
    }

    #[test]
    fn no_change_means_zero_new() {
        let mut c = RetentionCursor::new();
        let t = transcript();
        c.advance(&t).unwrap();
        assert_eq!(c.advance(&t), Some(0));
    }

    #[test]
    fn an_in_place_edit_resets_the_cursor() {
        let mut c = RetentionCursor::new();
        let mut t = transcript();
        c.advance(&t).unwrap();
        // Edit message 1 — same count, different content.
        t[1] = msg(MessageRole::Assistant, "edited reply");
        assert_eq!(c.advance(&t), None, "an in-place edit must reset");
        // The cursor now covers the edited transcript.
        assert_eq!(c.retained(), 3);
        assert_eq!(c.advance(&t), Some(0));
    }

    #[test]
    fn a_shrunk_transcript_resets_the_cursor() {
        let mut c = RetentionCursor::new();
        let mut t = transcript();
        c.advance(&t).unwrap();
        t.pop();
        assert_eq!(c.advance(&t), None, "a rewind must reset");
        assert_eq!(c.retained(), 2);
    }

    #[test]
    fn a_reset_then_append_is_incremental_again() {
        let mut c = RetentionCursor::new();
        let mut t = transcript();
        c.advance(&t).unwrap();
        t[0] = msg(MessageRole::User, "rewritten");
        assert_eq!(c.advance(&t), None);
        t.push(msg(MessageRole::Assistant, "after"));
        assert_eq!(c.advance(&t), Some(1));
    }

    #[test]
    fn clear_empties_the_cursor() {
        let mut c = RetentionCursor::new();
        c.advance(&transcript()).unwrap();
        c.clear();
        assert_eq!(c.retained(), 0);
        assert_eq!(c.hash(), PrefixHash::SEED);
    }

    #[test]
    fn a_role_change_changes_the_hash() {
        // Two transcripts differing only in a role must hash
        // differently — the chain feeds the role tag.
        let mut c = RetentionCursor::new();
        let t1 = vec![msg(MessageRole::User, "x")];
        c.advance(&t1).unwrap();
        let h1 = c.hash();
        let t2 = vec![msg(MessageRole::Assistant, "x")];
        let mut c2 = RetentionCursor::new();
        c2.advance(&t2).unwrap();
        assert_ne!(h1, c2.hash());
    }

    // ---- cadence -----------------------------------------------------

    #[test]
    fn the_default_cadence_is_five_turns_four_messages() {
        let c = RetentionCadence::default();
        assert_eq!(c.every_n_turns, 5);
        assert_eq!(c.min_new_messages, 4);
    }

    #[test]
    fn a_small_tail_is_not_due() {
        let c = RetentionCadence::default();
        // Too few messages, even with enough turns.
        assert!(!c.is_due(3, 5));
        // Enough messages, but too few user turns (the old dead-code bug
        // fired here: any tool-using turn reached 4 messages in 1 turn).
        assert!(!c.is_due(20, 4));
    }

    #[test]
    fn a_large_tail_is_due() {
        let c = RetentionCadence::default();
        assert!(c.is_due(4, 5));
        assert!(c.is_due(20, 7));
    }
}
