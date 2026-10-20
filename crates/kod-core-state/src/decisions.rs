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

#[cfg(test)]
mod decisions_proptests {
    //! Properties for `DecisionLog`.
    //!
    //! Four methods, pure except for the wall-clock timestamp in
    //! `push`. None of the properties below compare whole
    //! `DecisionRecord`s, so the embedded `created_at_ms` does not
    //! need a normalize helper.
    //!
    //! Contracts pinned:
    //! * `push` returns fresh ids and grows the log by one.
    //! * `drop` returns true iff the id was present; length arithmetic.
    //! * `recent(n)` returns `min(n, len)` items, newest first.
    //! * `render_prompt_block(n)` renders `min(n, len)` lines in
    //!   chronological order (oldest of the recent-n first), each
    //!   tagged by kind.
    //!
    //! Not covered here: `drop` uses `retain`, which removes *all*
    //! matching records. `push` cannot produce duplicate ids through
    //! the public API, so this is a no-op difference in practice —
    //! a control pins it in case a future caller sets `entries`
    //! directly (it is `pub`).

    use super::*;
    use proptest::prelude::*;

    fn any_kind() -> impl Strategy<Value = DecisionKind> {
        prop::sample::select(vec![
            DecisionKind::UserPreference,
            DecisionKind::Approach,
            DecisionKind::FileChange,
            DecisionKind::Constraint,
            DecisionKind::Other,
        ])
    }

    fn any_author() -> impl Strategy<Value = DecisionAuthor> {
        prop::sample::select(vec![DecisionAuthor::User, DecisionAuthor::Assistant])
    }

    fn arb_push_spec() -> impl Strategy<Value = (u64, DecisionKind, String, DecisionAuthor)> {
        (0u64..=100, any_kind(), "[a-z ]{0,20}", any_author())
    }

    proptest! {
        /// `push` returns fresh ids: across any sequence of pushes,
        /// the returned ids are all distinct.
        #[test]
        fn prop_push_ids_are_fresh(specs in prop::collection::vec(arb_push_spec(), 0..=8)) {
            let mut log = DecisionLog::default();
            let mut seen: std::collections::HashSet<u64> = std::collections::HashSet::new();
            for (turn_id, kind, text, author) in specs {
                let id = log.push(turn_id, kind, text, author);
                prop_assert!(seen.insert(id), "push returned a duplicate id: {}", id);
            }
        }

        /// `push` grows the log by exactly one.
        #[test]
        fn prop_push_grows_by_one(specs in prop::collection::vec(arb_push_spec(), 0..=8)) {
            let mut log = DecisionLog::default();
            for (i, (turn_id, kind, text, author)) in specs.into_iter().enumerate() {
                let before = log.entries.len();
                log.push(turn_id, kind, text, author);
                prop_assert_eq!(log.entries.len(), before + 1, "at push {}", i);
            }
        }

        /// `drop` of a present id returns true and shrinks the log by
        /// one; `drop` of an absent id returns false and leaves the
        /// log unchanged.
        #[test]
        fn prop_drop_returns_true_iff_present(
            specs in prop::collection::vec(arb_push_spec(), 1..=6),
        ) {
            let mut log = DecisionLog::default();
            let mut ids: Vec<u64> = Vec::new();
            for (turn_id, kind, text, author) in specs {
                ids.push(log.push(turn_id, kind, text, author));
            }
            // Drop the first id: returns true, shrinks by one.
            let first = ids[0];
            let before = log.entries.len();
            prop_assert!(log.drop(first), "drop of a present id returned false");
            prop_assert_eq!(log.entries.len(), before - 1);

            // Drop the same id again: returns false, no change.
            let after_first = log.entries.len();
            prop_assert!(!log.drop(first), "drop of an absent id returned true");
            prop_assert_eq!(log.entries.len(), after_first);

            // Drop a definitely-absent id (next_id + 1000).
            let absent = log.next_id + 1000;
            let before_absent = log.entries.len();
            prop_assert!(!log.drop(absent));
            prop_assert_eq!(log.entries.len(), before_absent);
        }

        /// `recent(n)` returns exactly `min(n, len)` items.
        #[test]
        fn prop_recent_returns_min(
            specs in prop::collection::vec(arb_push_spec(), 0..=8),
            n in 0usize..=10,
        ) {
            let mut log = DecisionLog::default();
            for (turn_id, kind, text, author) in specs {
                log.push(turn_id, kind, text, author);
            }
            let expected = std::cmp::min(n, log.entries.len());
            prop_assert_eq!(log.recent(n).len(), expected);
        }

        /// `recent(n)` returns newest first: the ids are in
        /// descending order.
        #[test]
        fn prop_recent_is_newest_first(
            specs in prop::collection::vec(arb_push_spec(), 1..=8),
            n in 1usize..=8,
        ) {
            let mut log = DecisionLog::default();
            for (turn_id, kind, text, author) in specs {
                log.push(turn_id, kind, text, author);
            }
            let recent = log.recent(n);
            for pair in recent.windows(2) {
                prop_assert!(
                    pair[0].id > pair[1].id,
                    "recent is not newest-first: {} then {}",
                    pair[0].id, pair[1].id,
                );
            }
        }

        /// `render_prompt_block(n)` for a non-empty log is:
        /// * the header line `## Decisions\\n\\n`;
        /// * `min(n, len)` bullet lines, one per recent entry;
        /// * in chronological order (oldest of the recent-n first).
        ///
        /// Counting by `- [` prefixes is robust to the tag choice.
        #[test]
        fn prop_render_line_count(
            specs in prop::collection::vec(arb_push_spec(), 0..=8),
            n in 0usize..=10,
        ) {
            let mut log = DecisionLog::default();
            for (turn_id, kind, text, author) in specs {
                log.push(turn_id, kind, text, author);
            }
            if log.entries.is_empty() {
                prop_assert_eq!(log.render_prompt_block(n), "");
                return Ok(());
            }
            let block = log.render_prompt_block(n);
            let bullets = block.lines().filter(|l| l.starts_with("- [")).count();
            prop_assert_eq!(bullets, std::cmp::min(n, log.entries.len()));
        }
    }

    /// Control: `render_prompt_block` for one entry contains the tag
    /// matching the kind. Pins the per-kind tag mapping; a new kind
    /// added to `DecisionKind` without updating `render_prompt_block`
    /// would fail to compile (exhaustive match), but a *wrong* tag
    /// (e.g. `FileChange` rendering as "preference") would pass a
    /// length-only property. This pins the mapping per kind.
    #[test]
    fn control_render_tag_matches_kind() {
        let cases = [
            (DecisionKind::UserPreference, "preference"),
            (DecisionKind::Approach, "approach"),
            (DecisionKind::FileChange, "file"),
            (DecisionKind::Constraint, "constraint"),
            (DecisionKind::Other, "other"),
        ];
        for (kind, expected_tag) in cases {
            let mut log = DecisionLog::default();
            log.push(0, kind, "x".to_string(), DecisionAuthor::User);
            let block = log.render_prompt_block(1);
            assert!(
                block.contains(&format!("[{expected_tag}]")),
                "kind {:?} should render as [{expected_tag}], got: {block}",
                kind,
            );
        }
    }

    /// Control: `drop` uses `retain`, which removes **all** records
    /// with the matching id. `push` cannot produce duplicate ids
    /// through the public API, but `entries` is `pub` — a caller
    /// who sets it directly can. The control pins the "drops every
    /// matching record" behavior so a future change to
    /// "drops the first match" is a visible decision.
    #[test]
    fn control_drop_removes_all_matching_records() {
        let record = DecisionRecord {
            id: 42,
            turn_id: 0,
            kind: DecisionKind::Other,
            text: "x".to_string(),
            author: DecisionAuthor::User,
            created_at_ms: 0,
            tags: Vec::new(),
        };
        let mut log = DecisionLog::default();
        log.entries.push(record.clone());
        log.entries.push(record.clone());
        log.entries.push(record);
        assert_eq!(log.entries.len(), 3);
        assert!(log.drop(42));
        assert_eq!(
            log.entries.len(),
            0,
            "drop must remove all matching records"
        );
    }

    /// Control: `next_id` is not decremented or reused by `drop`. A
    /// caller dropping the newest record and then pushing gets a
    /// fresh id greater than the dropped one.
    #[test]
    fn control_next_id_never_reused_after_drop() {
        let mut log = DecisionLog::default();
        let a = log.push(
            0,
            DecisionKind::Other,
            "a".to_string(),
            DecisionAuthor::User,
        );
        let b = log.push(
            0,
            DecisionKind::Other,
            "b".to_string(),
            DecisionAuthor::User,
        );
        assert!(b > a);
        log.drop(b);
        let c = log.push(
            0,
            DecisionKind::Other,
            "c".to_string(),
            DecisionAuthor::User,
        );
        assert!(c > b, "next_id reused a dropped id: c={} b={}", c, b);
    }
}
