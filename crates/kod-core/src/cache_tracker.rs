//! Client-side cache validation.
//!
//! The provider reports a cache miss (`cache_read = 0`); it does not
//! report *why*. A miss caused by compaction is expected, a miss
//! caused by a harness bug is not, and the two look identical in the
//! usage counters. This module watches the transcript the client
//! sends and raises when the prefix stops growing append-only — the
//! one signature every legitimate cache break shares, and every bug
//! shares too. Paired with the invalidation journal, it makes a miss
//! *explainable*: a violation with a matching journal entry is the
//! break you made on purpose; a violation with an empty journal is a
//! bug you just found.
//!
//! Not a replacement for the golden-prefix tests. Those pin bytes at
//! build time; this detects drift at runtime, in the request that
//! actually goes out.

use kod_types::{ChatMessage, MessageRole};

/// A prefix that stopped being append-only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheViolation {
    /// Index of the first message that differs from what was seen
    /// before.
    pub turn: usize,
    pub reason: String,
}

impl std::fmt::Display for CacheViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "cache violation at message {}: {}", self.turn, self.reason)
    }
}

/// FNV-1a over the cache-relevant projection of a transcript.
pub struct CacheTracker {
    hashes: Vec<u64>,
    /// How many leading messages were dropped when the history was
    /// trimmed. `hashes[i]` corresponds to `messages[i + dropped]`,
    /// so a bounded tracker still compares the right positions.
    dropped: usize,
    /// Bound so a very long session does not hold a hash per message
    /// forever. Past this the oldest hashes are dropped, which makes
    /// the tracker blind to mutations that far back — a mutation that
    /// deep is already a full-prefix miss regardless.
    max_history: usize,
}

impl Default for CacheTracker {
    fn default() -> Self {
        // 4096 messages: far more than a single request's transcript.
        Self::new(4096)
    }
}

impl CacheTracker {
    pub fn new(max_history: usize) -> Self {
        Self {
            hashes: Vec::new(),
            dropped: 0,
            max_history: max_history.max(1),
        }
    }

    /// Observe the transcript about to be sent.
    ///
    /// Returns `Err` on the first position where the transcript is no
    /// longer a prefix-extension of what was observed before.
    pub fn observe(&mut self, messages: &[ChatMessage]) -> Result<(), CacheViolation> {
        for (i, m) in messages.iter().enumerate() {
            // The retained window starts at `dropped`; anything before
            // it was trimmed and cannot be compared.
            let Some(slot) = i.checked_sub(self.dropped) else {
                continue;
            };
            let h = stable_message_hash(m);
            match self.hashes.get(slot) {
                // A new message: append-only growth, the good case.
                None => self.hashes.push(h),
                Some(&prev) if prev == h => {}
                Some(_) => {
                    return Err(CacheViolation {
                        turn: i,
                        reason: "message content changed".to_string(),
                    });
                }
            }
        }
        let observed_total = messages.len();
        let tracked_total = self.dropped + self.hashes.len();
        if observed_total < tracked_total {
            // The transcript shrank: a compaction, a truncation, or a
            // history reset. Legitimate when journalled, a bug when not.
            return Err(CacheViolation {
                turn: observed_total,
                reason: format!(
                    "transcript shrank from {tracked_total} to {observed_total} messages",
                ),
            });
        }
        // Bound the history. Trimming records the offset so the next
        // observation compares the right positions.
        if self.hashes.len() > self.max_history {
            let drop = self.hashes.len() - self.max_history;
            self.hashes.drain(..drop);
            self.dropped += drop;
        }
        Ok(())
    }

    /// Forget everything observed so far.
    ///
    /// Called after a *documented* prefix break — a compaction, a
    /// model switch, a tool-surface rebuild — so the next observation
    /// starts from a fresh baseline instead of reporting the break
    /// you already know about.
    pub fn reset(&mut self) {
        self.hashes.clear();
        self.dropped = 0;
    }

    /// How many messages have been observed since the last reset.
    /// Total messages seen since the last reset, including those
    /// trimmed from the window.
    pub fn observed(&self) -> usize {
        self.dropped + self.hashes.len()
    }

    /// Messages still held in the comparison window. Past this the
    /// oldest are dropped — a mutation that deep is a full-prefix
    /// miss regardless, so the tracker does not need them.
    pub fn retained(&self) -> usize {
        self.hashes.len()
    }
}

/// FNV-1a over the parts of a message the provider's cache depends on.
///
/// `MessageMetadata` is deliberately excluded: `pinned` and the
/// timestamps change for reasons that have nothing to do with the
/// wire bytes, and folding them in would report a violation every
/// time a user pinned a turn.
pub fn stable_message_hash(m: &ChatMessage) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h = OFFSET;
    let mut feed = |bytes: &[u8]| {
        for b in bytes {
            h ^= *b as u64;
            h = h.wrapping_mul(PRIME);
        }
    };
    let role_tag: u8 = match m.role {
        MessageRole::User => 1,
        MessageRole::Assistant => 2,
        MessageRole::System => 3,
        MessageRole::Tool => 4,
        MessageRole::Agent(_) => 5,
    };
    feed(&[role_tag]);
    feed(m.content.as_bytes());
    for c in &m.tool_calls {
        if let Some(id) = &c.id {
            feed(id.as_bytes());
        }
        feed(c.tool_name.as_bytes());
    }
    if let Some(id) = &m.tool_call_id {
        feed(id.as_bytes());
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use kod_types::{MessageId, ToolCall};
    use time::OffsetDateTime;

    fn msg(role: MessageRole, content: &str) -> ChatMessage {
        ChatMessage::text(MessageId::new(), role, content, OffsetDateTime::UNIX_EPOCH)
    }

    #[test]
    fn append_only_growth_is_accepted() {
        let mut t = CacheTracker::new(10);
        let a = vec![msg(MessageRole::User, "one")];
        assert!(t.observe(&a).is_ok());
        let mut b = a.clone();
        b.push(msg(MessageRole::Assistant, "two"));
        assert!(t.observe(&b).is_ok());
        assert_eq!(t.observed(), 2);
    }

    #[test]
    fn a_content_change_is_a_violation() {
        let mut t = CacheTracker::new(10);
        t.observe(&[msg(MessageRole::User, "one")]).unwrap();
        let mutated = vec![msg(MessageRole::User, "ONE")];
        let err = t.observe(&mutated).unwrap_err();
        assert_eq!(err.turn, 0);
        assert!(err.reason.contains("changed"));
    }

    #[test]
    fn a_shrinking_transcript_is_a_violation() {
        let mut t = CacheTracker::new(10);
        let three = vec![
            msg(MessageRole::User, "a"),
            msg(MessageRole::Assistant, "b"),
            msg(MessageRole::User, "c"),
        ];
        t.observe(&three).unwrap();
        let err = t.observe(&three[..2]).unwrap_err();
        assert_eq!(err.turn, 2);
        assert!(err.reason.contains("shrank"));
    }

    #[test]
    fn pinning_a_turn_is_not_a_violation() {
        // Metadata is excluded from the hash on purpose: pinning
        // changes `metadata.pinned`, not the wire bytes.
        let mut t = CacheTracker::new(10);
        let mut a = msg(MessageRole::User, "one");
        t.observe(std::slice::from_ref(&a)).unwrap();
        a.metadata.pinned = true;
        assert!(t.observe(std::slice::from_ref(&a)).is_ok());
    }

    #[test]
    fn reset_clears_the_baseline() {
        let mut t = CacheTracker::new(10);
        t.observe(&[msg(MessageRole::User, "one")]).unwrap();
        t.reset();
        assert_eq!(t.observed(), 0);
        // After a reset, a totally different transcript is fine.
        assert!(t.observe(&[msg(MessageRole::Assistant, "different")]).is_ok());
    }

    #[test]
    fn the_history_is_bounded() {
        let mut t = CacheTracker::new(3);
        let mut msgs: Vec<ChatMessage> = Vec::new();
        for i in 0..10 {
            msgs.push(msg(MessageRole::User, &format!("m{i}")));
            t.observe(&msgs).unwrap();
        }
        assert!(t.retained() <= 3, "history window is capped at max_history");
        assert_eq!(t.observed(), 10, "observed counts every message seen");
    }

    #[test]
    fn a_tool_call_id_change_is_a_violation() {
        let mut a = msg(MessageRole::Assistant, "");
        a.tool_calls.push(ToolCall {
            id: Some("c1".into()),
            tool_name: "read_file".into(),
            arguments: serde_json::json!({}),
        });
        let mut t = CacheTracker::new(10);
        t.observe(std::slice::from_ref(&a)).unwrap();

        a.tool_calls[0].id = Some("c2".into());
        assert!(t.observe(std::slice::from_ref(&a)).is_err());
    }

    #[test]
    fn hashing_is_stable_across_calls() {
        let m = msg(MessageRole::User, "same");
        assert_eq!(stable_message_hash(&m), stable_message_hash(&m));
    }
}
