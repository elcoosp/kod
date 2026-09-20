//! Durable decisions log (Tier 3.4).
//!
//! Alongside raw history, KOD keeps a compact list of the durable
//! choices a session made. The prompt builder includes recent
//! decisions even after the raw turn has aged out of the FIFO
//! truncation window, so the model does not re-derive decisions it
//! already made.
//!
//! Extraction is a byproduct of the Jev `is_decision` classifier used
//! elsewhere (Tier 2.2 handoff, P4.8). This module owns the storage
//! and rendering; the classifier lives in `engine.rs`.

use serde::{Deserialize, Serialize};

/// One durable decision.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionRecord {
    pub id: u64,
    /// Turn id that produced this decision.
    pub turn_id: u64,
    pub kind: DecisionKind,
    pub text: String,
    pub author: DecisionAuthor,
    pub created_at_ms: u64,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DecisionKind {
    /// A preference the user stated ("use tabs, not spaces").
    UserPreference,
    /// An approach the pair chose ("use `similar` for the diff").
    Approach,
    /// A file change the pair decided ("add `Tier 3.4` to the plan").
    FileChange,
    /// A constraint the pair accepted ("do not add new deps").
    Constraint,
    /// Anything the classifier flagged that does not fit the above.
    #[default]
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DecisionAuthor {
    #[default]
    User,
    Assistant,
}

/// The per-session log. Keyed by transcript so a swarm agent's
/// decisions do not bleed into the interactive session's prompt.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DecisionLog {
    pub next_id: u64,
    pub entries: Vec<DecisionRecord>,
}

impl DecisionLog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a decision. Returns the assigned id.
    pub fn push(
        &mut self,
        turn_id: u64,
        kind: DecisionKind,
        text: String,
        author: DecisionAuthor,
    ) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.entries.push(DecisionRecord {
            id,
            turn_id,
            kind,
            text,
            author,
            created_at_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
            tags: Vec::new(),
        });
        id
    }

    /// Drop a decision by id. Returns `true` when something was
    /// removed.
    pub fn drop(&mut self, id: u64) -> bool {
        let before = self.entries.len();
        self.entries.retain(|d| d.id != id);
        self.entries.len() != before
    }

    /// Most recent N entries, in reverse-chronological order.
    pub fn recent(&self, n: usize) -> Vec<&DecisionRecord> {
        self.entries.iter().rev().take(n).collect()
    }

    /// Render the recent decisions as a prompt block. Kept compact —
    /// the model does not need the full text of every decision, just
    /// the durable facts.
    pub fn render_prompt_block(&self, n: usize) -> String {
        if self.entries.is_empty() {
            return String::new();
        }
        let recent = self.recent(n);
        let mut out = String::from("## Decisions\n\n");
        // Chronological order in the prompt so the most recent is at
        // the bottom, matching how the model reads.
        for d in recent.iter().rev() {
            let tag = match d.kind {
                DecisionKind::UserPreference => "preference",
                DecisionKind::Approach => "approach",
                DecisionKind::FileChange => "file",
                DecisionKind::Constraint => "constraint",
                DecisionKind::Other => "other",
            };
            out.push_str(&format!("- [{tag}] {}\n", d.text));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_log_is_empty() {
        let l = DecisionLog::new();
        assert!(l.entries.is_empty());
        assert_eq!(l.next_id, 0);
    }

    #[test]
    fn push_assigns_monotonic_ids() {
        let mut l = DecisionLog::new();
        let a = l.push(
            1,
            DecisionKind::Approach,
            "x".into(),
            DecisionAuthor::Assistant,
        );
        let b = l.push(
            2,
            DecisionKind::Constraint,
            "y".into(),
            DecisionAuthor::User,
        );
        assert_eq!(a, 0);
        assert_eq!(b, 1);
        assert_eq!(l.next_id, 2);
    }

    #[test]
    fn drop_removes_by_id() {
        let mut l = DecisionLog::new();
        l.push(1, DecisionKind::Other, "x".into(), DecisionAuthor::User);
        l.push(2, DecisionKind::Other, "y".into(), DecisionAuthor::User);
        assert!(l.drop(0));
        assert_eq!(l.entries.len(), 1);
        assert!(!l.drop(99));
    }

    #[test]
    fn recent_returns_newest_first() {
        let mut l = DecisionLog::new();
        for i in 0..5 {
            l.push(
                1,
                DecisionKind::Other,
                format!("d{i}"),
                DecisionAuthor::User,
            );
        }
        let r = l.recent(3);
        assert_eq!(r[0].text, "d4");
        assert_eq!(r[2].text, "d2");
    }

    #[test]
    fn render_block_is_empty_when_empty() {
        let l = DecisionLog::new();
        assert!(l.render_prompt_block(10).is_empty());
    }

    #[test]
    fn render_block_lists_recent_in_chronological_order() {
        let mut l = DecisionLog::new();
        l.push(
            1,
            DecisionKind::Approach,
            "first".into(),
            DecisionAuthor::User,
        );
        l.push(
            2,
            DecisionKind::Approach,
            "second".into(),
            DecisionAuthor::User,
        );
        let b = l.render_prompt_block(10);
        let first = b.find("first").unwrap();
        let second = b.find("second").unwrap();
        assert!(first < second);
        assert!(b.contains("## Decisions"));
    }

    #[test]
    fn render_block_limits_to_n() {
        let mut l = DecisionLog::new();
        for i in 0..10 {
            l.push(
                1,
                DecisionKind::Other,
                format!("d{i}"),
                DecisionAuthor::User,
            );
        }
        let b = l.render_prompt_block(3);
        assert!(b.contains("d9"));
        assert!(!b.contains("d0\n"));
    }

    #[test]
    fn round_trip_through_json() {
        let mut l = DecisionLog::new();
        l.push(
            1,
            DecisionKind::Constraint,
            "no new deps".into(),
            DecisionAuthor::User,
        );
        let s = serde_json::to_string(&l).unwrap();
        let back: DecisionLog = serde_json::from_str(&s).unwrap();
        assert_eq!(back.entries.len(), 1);
        assert_eq!(back.entries[0].text, "no new deps");
    }

    #[test]
    fn decision_kind_round_trips() {
        for k in [
            DecisionKind::UserPreference,
            DecisionKind::Approach,
            DecisionKind::FileChange,
            DecisionKind::Constraint,
            DecisionKind::Other,
        ] {
            let s = serde_json::to_string(&k).unwrap();
            let back: DecisionKind = serde_json::from_str(&s).unwrap();
            assert_eq!(k, back);
        }
    }

    #[test]
    fn decision_author_round_trips() {
        for a in [DecisionAuthor::User, DecisionAuthor::Assistant] {
            let s = serde_json::to_string(&a).unwrap();
            let back: DecisionAuthor = serde_json::from_str(&s).unwrap();
            assert_eq!(a, back);
        }
    }
}
