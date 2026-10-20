//! Shared blackboard for swarm coordination (Tier 3.5).
//!
//! Agents coordinate by message-passing today (`AgentCommunicationHub`).
//! That is fine for a small fan-out but forces the planner to
//! enumerate every dependency upfront. A shared key-value store with
//! provenance lets agents publish findings and read each other's,
//! so the planner can be lazy about the shape of the work.
//!
//! Auto-populated by the engine on every write, every file read,
//! and every completed subtask. Read access is free; write access is
//! per-agent so a later reader can tell who said what.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;

/// The type of producer that wrote an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthorKind {
    /// The engine auto-populated this entry (claim, file summary, …).
    Engine,
    /// A specific swarm agent authored it.
    Agent,
    /// The planner or coordinator.
    Coordinator,
    /// The user typed it.
    User,
}

/// One entry on the blackboard.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlackboardEntry {
    pub key: String,
    pub value: serde_json::Value,
    pub author: String,
    pub author_kind: AuthorKind,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Prior values, newest first. Bounded to 16 to keep the map
    /// small even under a chatty agent.
    #[serde(default)]
    pub history: Vec<(u64, serde_json::Value)>,
}

/// The board. Clone shares state.
#[derive(Clone, Default)]
pub struct Blackboard {
    inner: Arc<parking_lot::RwLock<BTreeMap<String, BlackboardEntry>>>,
}

impl std::fmt::Debug for Blackboard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let len = self.inner.read().len();
        f.debug_struct("Blackboard").field("entries", &len).finish()
    }
}

impl Blackboard {
    pub fn new() -> Self {
        Self::default()
    }

    /// Put a value. Overwrites by key; the old value is retained in
    /// `history`.
    pub fn put(
        &self,
        key: impl Into<String>,
        value: serde_json::Value,
        author: impl Into<String>,
        author_kind: AuthorKind,
        tags: Vec<String>,
    ) {
        let key = key.into();
        let author = author.into();
        let now = now_ms();
        let mut g = self.inner.write();
        let entry = g.entry(key.clone()).or_insert_with(|| BlackboardEntry {
            key: key.clone(),
            value: value.clone(),
            author: author.clone(),
            author_kind,
            created_at_ms: now,
            updated_at_ms: now,
            tags: tags.clone(),
            history: Vec::new(),
        });
        if entry.value != value {
            entry
                .history
                .insert(0, (entry.updated_at_ms, entry.value.clone()));
            entry.history.truncate(16);
            entry.value = value;
            entry.author = author;
            entry.author_kind = author_kind;
            entry.tags = tags;
            entry.updated_at_ms = now;
        }
    }

    /// Get a value by key.
    pub fn get(&self, key: &str) -> Option<BlackboardEntry> {
        self.inner.read().get(key).cloned()
    }

    /// All entries that carry `tag`.
    pub fn query_tag(&self, tag: &str) -> Vec<BlackboardEntry> {
        self.inner
            .read()
            .values()
            .filter(|e| e.tags.iter().any(|t| t == tag))
            .cloned()
            .collect()
    }

    /// All entries whose key begins with `prefix`.
    pub fn query_prefix(&self, prefix: &str) -> Vec<BlackboardEntry> {
        self.inner
            .read()
            .iter()
            .filter(|(k, _)| k.starts_with(prefix))
            .map(|(_, v)| v.clone())
            .collect()
    }

    /// Every entry, for `/agents blackboard`.
    pub fn all(&self) -> Vec<BlackboardEntry> {
        self.inner.read().values().cloned().collect()
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.inner.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.read().is_empty()
    }

    /// Drop everything. Called between swarm runs.
    pub fn clear(&self) {
        self.inner.write().clear();
    }

    /// Render a compact block for an agent's prompt: the current
    /// state of everything tagged `team`, up to a budget. Bounded so
    /// a chatty swarm cannot blow the context window.
    pub fn render_prompt_block(&self, tag: &str, max_entries: usize, max_chars: usize) -> String {
        let entries = self.query_tag(tag);
        if entries.is_empty() {
            return String::new();
        }
        let mut out = String::from("## Team knowledge\n\n");
        for e in entries.iter().take(max_entries) {
            let line = format!("- {}: {}\n", e.key, e.value);
            // The parameter is `max_chars`, so count chars. A byte
            // count truncates a CJK block at roughly a third of
            // the visible budget (each CJK char is 3 bytes).
            if out.chars().count() + line.chars().count() > max_chars {
                break;
            }
            out.push_str(&line);
        }
        out
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

    #[test]
    fn new_board_is_empty() {
        let b = Blackboard::new();
        assert_eq!(b.len(), 0);
        assert!(b.is_empty());
    }

    #[test]
    fn put_then_get() {
        let b = Blackboard::new();
        b.put(
            "claim:a:src",
            serde_json::json!({"glob": "src/**"}),
            "a",
            AuthorKind::Agent,
            vec!["claim".into()],
        );
        let e = b.get("claim:a:src").unwrap();
        assert_eq!(e.author, "a");
        assert_eq!(e.value["glob"], "src/**");
    }

    #[test]
    fn overwrite_retains_history() {
        let b = Blackboard::new();
        b.put("k", serde_json::json!(1), "a", AuthorKind::Agent, vec![]);
        b.put("k", serde_json::json!(2), "a", AuthorKind::Agent, vec![]);
        let e = b.get("k").unwrap();
        assert_eq!(e.value, serde_json::json!(2));
        assert_eq!(e.history.len(), 1);
        assert_eq!(e.history[0].1, serde_json::json!(1));
    }

    #[test]
    fn same_value_does_not_grow_history() {
        let b = Blackboard::new();
        b.put("k", serde_json::json!(1), "a", AuthorKind::Agent, vec![]);
        b.put("k", serde_json::json!(1), "a", AuthorKind::Agent, vec![]);
        let e = b.get("k").unwrap();
        assert!(e.history.is_empty());
    }

    #[test]
    fn query_by_tag() {
        let b = Blackboard::new();
        b.put(
            "a",
            serde_json::json!(1),
            "x",
            AuthorKind::Engine,
            vec!["t".into()],
        );
        b.put(
            "b",
            serde_json::json!(2),
            "x",
            AuthorKind::Engine,
            vec!["u".into()],
        );
        assert_eq!(b.query_tag("t").len(), 1);
        assert_eq!(b.query_tag("u").len(), 1);
    }

    #[test]
    fn query_by_prefix() {
        let b = Blackboard::new();
        b.put(
            "claim:a:1",
            serde_json::json!(1),
            "a",
            AuthorKind::Agent,
            vec![],
        );
        b.put(
            "claim:b:2",
            serde_json::json!(2),
            "b",
            AuthorKind::Agent,
            vec![],
        );
        b.put(
            "other",
            serde_json::json!(3),
            "c",
            AuthorKind::Agent,
            vec![],
        );
        assert_eq!(b.query_prefix("claim:").len(), 2);
    }

    #[test]
    fn clear_empties() {
        let b = Blackboard::new();
        b.put("k", serde_json::json!(1), "a", AuthorKind::Agent, vec![]);
        b.clear();
        assert!(b.is_empty());
    }

    #[test]
    fn render_block_is_empty_when_no_match() {
        let b = Blackboard::new();
        assert!(b.render_prompt_block("team", 10, 1000).is_empty());
    }

    #[test]
    fn render_block_respects_max_chars() {
        let b = Blackboard::new();
        for i in 0..50 {
            b.put(
                format!("k{i}"),
                serde_json::json!(format!("value-{i}")),
                "x",
                AuthorKind::Agent,
                vec!["team".into()],
            );
        }
        let out = b.render_prompt_block("team", 100, 200);
        assert!(out.len() <= 250, "out is {} chars", out.len());
    }

    #[test]
    fn clone_shares_state() {
        let a = Blackboard::new();
        let b = a.clone();
        a.put("k", serde_json::json!(1), "a", AuthorKind::Agent, vec![]);
        assert_eq!(b.get("k").unwrap().value, serde_json::json!(1));
    }
}

#[cfg(test)]
mod render_block_proptests {
    //! Properties for `Blackboard::render_prompt_block`'s char bound.
    //!
    //! The function's parameter is `max_chars`. The loop accumulates
    //! `out` with `format!("- {}: {}\n", e.key, e.value)` and breaks
    //! when `out.len() + line.len() > max_chars`. Both `.len()`
    //! calls are byte counts. For ASCII the two agree; for CJK a
    //! 3-byte character counts as 3, so the truncated block holds
    //! roughly a third of the visible characters the caller asked
    //! for.
    //!
    //! The property below sizes the CJK value so its byte length
    //! exceeds a 200-char budget but its char length does not. On
    //! the pre-fix code the byte comparison trips and drops the
    //! line; post-fix the char comparison keeps it.

    use super::*;
    use proptest::prelude::*;

    fn cjk_char() -> impl Strategy<Value = char> {
        prop::sample::select(vec![
            '中', '国', '建', '設', '計', '師', '文', '字', '力', '心',
        ])
    }

    fn cjk_string(max_len: usize) -> impl Strategy<Value = String> {
        prop::collection::vec(cjk_char(), 1..=max_len).prop_map(|chars| chars.into_iter().collect())
    }

    proptest! {
        /// A CJK value whose byte length exceeds 200 but whose char
        /// length (plus the wrapper) fits 200 must be present.
        /// Pre-fix the byte comparison drops it.
        #[test]
        fn prop_render_block_keeps_cjk_under_char_budget(
            body in cjk_string(100),
        ) {
            // Need char count of value + wrapper + header under 200,
            // but byte count over 200.
            //   header: "## Team knowledge\n\n" = 19 chars
            //   line: "- team: " = 8 chars, "\n" = 1 char
            //   JSON wrapper around the value: {"k":"","v":""} adds
            //   ~16 ASCII chars (the CJK body is the value)
            // So chars ≈ 19 + 9 + 16 + body.chars().count() = 44 + n
            //     bytes ≈ 44 + 3*n (for CJK)
            // Need: 44 + n <= 200  (n <= 156)
            //       44 + 3n > 200  (n > 52)
            prop_assume!(body.chars().count() > 55 && body.chars().count() <= 100);

            let b = Blackboard::new();
            b.put(
                "team",
                serde_json::json!(body),
                "test",
                AuthorKind::Agent,
                vec!["team".to_string()],
            );
            let out = b.render_prompt_block("team", 10, 200);
            prop_assert!(
                out.contains(&body),
                "CJK value of {} chars ({} bytes) under a 200-char budget must be present; output is {} chars: {:?}",
                body.chars().count(), body.len(), out.chars().count(), out,
            );
        }
    }

    /// Control: a short ASCII entry under a generous budget is present.
    #[test]
    fn control_short_ascii_entry_fits() {
        let b = Blackboard::new();
        b.put(
            "team",
            serde_json::json!("v"),
            "test",
            AuthorKind::Agent,
            vec!["team".to_string()],
        );
        let out = b.render_prompt_block("team", 10, 200);
        assert!(
            out.contains("- team:"),
            "short ASCII entry should appear: {:?}",
            out
        );
    }

    /// Control: an oversized ASCII entry under a tight budget is
    /// truncated. The fix must not remove truncation entirely.
    #[test]
    fn control_oversized_ascii_entry_is_truncated() {
        let b = Blackboard::new();
        let long_value = "x".repeat(300);
        b.put(
            "team",
            serde_json::json!(long_value),
            "test",
            AuthorKind::Agent,
            vec!["team".to_string()],
        );
        let out = b.render_prompt_block("team", 10, 100);
        assert!(
            out.chars().count() <= 100,
            "ASCII output {} chars over 100",
            out.chars().count(),
        );
        assert!(out.contains("## Team knowledge"));
    }
}
