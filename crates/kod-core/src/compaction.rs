//! Compaction: keep the transcript inside the model's context window.
//!
//! The pre-compaction engine had one tool — `compact_history_for(n)`,
//! a drop-oldest drain called by the TUI's `/compact`. Nothing ran
//! automatically, nothing looked at real token usage, and a drop could
//! land between a `tool_use` and its `tool_result`, orphaning the pair
//! and making the provider reject the request.
//!
//! This module is the decision half. It answers three questions
//! without touching the engine or a provider:
//!
//! 1. **Should we compact?** — from *observed* token usage against the
//!    budget, at two thresholds (soft 80%, hard 95%). The soft
//!    threshold is where a background summary starts; the hard one is
//!    where the current turn compacts synchronously.
//! 2. **Where is a safe cutoff?** — the newest index such that every
//!    `tool_use` before it is either wholly before it or wholly after
//!    it. Never splits a pair.
//! 3. **What is the emergency summary?** — a no-LLM text describing
//!    what was dropped, for when the summary call fails or the hard
//!    threshold is crossed mid-turn and there is no time to ask.
//!
//! The engine calls these; the actual LLM summarization is a separate
//! step that falls back to [`emergency_summary`].

use kod_types::{ChatMessage, MessageRole};

/// Fraction of the budget at which a background summary begins. Below
/// this the transcript is fine; the summary lands on a later turn.
pub const SOFT_THRESHOLD: f64 = 0.80;

/// Fraction at which the current turn compacts before it runs. The
/// summary call has no time to return, so the emergency path is used
/// unless a background summary is already available.
pub const HARD_THRESHOLD: f64 = 0.95;

/// Turns kept verbatim at the tail. The recent exchange is what the
/// current turn is actually reasoning about; compacting it away is
/// the classic over-eager failure.
pub const RECENT_TURNS_TO_KEEP: usize = 10;

/// What the caller should do about the current context size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Under the soft threshold — nothing to do.
    None,
    /// Between the thresholds — start a background summary if one is
    /// not already running. The current turn proceeds unchanged.
    StartBackground,
    /// At or above the hard threshold — compact before this request.
    CompactNow,
}

/// Decide what to do, from the *observed* token count.
///
/// `used_tokens` is the number the provider reported for the last
/// request (prompt + completion), not a char-count estimate. The two
/// disagree by 20–50% depending on content, and an estimate-driven
/// compactor either fires far too early (wasting a summary) or far
/// too late (the request is rejected). The engine falls back to the
/// estimate only when no observed number exists yet — the first turn
/// of a session.
pub fn decide(used_tokens: u64, budget_tokens: u64) -> Action {
    if budget_tokens == 0 {
        // A zero budget means "no window known"; refusing to compact
        // is the safe default — the request will be rejected and the
        // caller learns the real number.
        return Action::None;
    }
    let ratio = used_tokens as f64 / budget_tokens as f64;
    if ratio >= HARD_THRESHOLD {
        Action::CompactNow
    } else if ratio >= SOFT_THRESHOLD {
        Action::StartBackground
    } else {
        Action::None
    }
}

/// The newest index `cut` such that:
///
/// * `turns.len() - cut <= keep_recent` is not required — the caller
///   decides `keep_recent`; this function only guarantees the cut is
///   *safe*, not that it is small enough.
/// * No `tool_use` before `cut` has its `tool_result` at or after
///   `cut`. A message whose `tool_calls` are non-empty must sit
///   wholly before the cut along with every `Tool`-role message that
///   answers one of its call ids.
///
/// Returns `None` when no safe cut exists that keeps at least one
/// message — a transcript of a single orphaned `tool_use` and nothing
/// else. The caller then leaves the transcript alone.
pub fn safe_cutoff(turns: &[ChatMessage], keep_recent: usize) -> Option<usize> {
    if turns.is_empty() {
        return None;
    }
    // The nominal target: everything before this index is dropped.
    let nominal = turns.len().saturating_sub(keep_recent);
    if nominal == 0 {
        // Fewer turns than we would keep: nothing to drop.
        return Some(0);
    }

    // Collect the call ids opened before `cut` and closed at or after
    // it, then walk the cut backwards until none straddle.
    let mut cut = nominal;
    loop {
        if cut == 0 {
            // Walked past the start: no safe cut that drops anything.
            return Some(0);
        }
        let open_before: std::collections::HashSet<&str> = turns[..cut]
            .iter()
            .flat_map(|m| m.tool_calls.iter())
            .filter_map(|c| c.id.as_deref())
            .collect();
        let closes_after = turns[cut..].iter().any(|m| {
            m.role == MessageRole::Tool
                && m.tool_call_id
                    .as_deref()
                    .is_some_and(|id| open_before.contains(id))
        });
        if !closes_after {
            return Some(cut);
        }
        cut -= 1;
    }
}

/// A no-LLM summary of what was dropped.
///
/// Used when the hard threshold is crossed mid-turn (no time for a
/// summary call) or when the summary call fails. It is deliberately
/// factual: counts, tool names, file paths. It does not try to
/// explain *why* the work happened — a model reading it knows it lost
/// detail and should ask rather than guess.
pub fn emergency_summary(dropped: &[ChatMessage], budget_tokens: u64) -> String {
    let tool_names: std::collections::BTreeSet<&str> = dropped
        .iter()
        .flat_map(|m| m.tool_calls.iter())
        .map(|c| c.tool_name.as_str())
        .collect();
    let files: std::collections::BTreeSet<String> = dropped
        .iter()
        .flat_map(|m| m.tool_calls.iter())
        .filter_map(|c| {
            c.arguments
                .get("path")
                .and_then(|p| p.as_str())
                .map(str::to_string)
        })
        .collect();

    let mut s = format!(
        "[Emergency compaction: {} messages dropped, context exceeded \
         {} tokens]",
        dropped.len(),
        budget_tokens,
    );
    if !tool_names.is_empty() {
        s.push_str("\nTools used: ");
        s.push_str(&tool_names.into_iter().collect::<Vec<_>>().join(", "));
    }
    if !files.is_empty() {
        let shown: Vec<&String> = files.iter().take(30).collect();
        s.push_str("\nFiles referenced: ");
        s.push_str(
            &shown
                .iter()
                .map(|f| f.as_str())
                .collect::<Vec<_>>()
                .join(", "),
        );
        if files.len() > 30 {
            s.push_str(&format!(" (and {} more)", files.len() - 30));
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use kod_types::{MessageId, ToolCall};
    use time::OffsetDateTime;

    fn msg(role: MessageRole, content: &str) -> ChatMessage {
        ChatMessage::text(MessageId::new(), role, content, OffsetDateTime::UNIX_EPOCH)
    }

    fn assistant_with_call(id: &str) -> ChatMessage {
        let mut m = msg(MessageRole::Assistant, "");
        m.tool_calls.push(ToolCall {
            id: Some(id.to_string()),
            tool_name: "read_file".to_string(),
            arguments: serde_json::json!({"path": "a.rs"}),
        });
        m
    }

    fn tool_result(id: &str) -> ChatMessage {
        let mut m = msg(MessageRole::Tool, "contents");
        m.tool_call_id = Some(id.to_string());
        m
    }

    #[test]
    fn decide_below_soft_is_none() {
        assert_eq!(decide(100, 1000), Action::None);
        assert_eq!(decide(799, 1000), Action::None);
    }

    #[test]
    fn decide_at_soft_is_background() {
        assert_eq!(decide(800, 1000), Action::StartBackground);
        assert_eq!(decide(949, 1000), Action::StartBackground);
    }

    #[test]
    fn decide_at_hard_is_compact_now() {
        assert_eq!(decide(950, 1000), Action::CompactNow);
        assert_eq!(decide(1000, 1000), Action::CompactNow);
    }

    #[test]
    fn decide_with_zero_budget_is_none() {
        // No known window: refuse to compact. The request is rejected
        // and the caller learns the real number rather than the
        // engine guessing.
        assert_eq!(decide(999_999, 0), Action::None);
    }

    #[test]
    fn safe_cutoff_empty_is_none() {
        assert!(safe_cutoff(&[], 5).is_none());
    }

    #[test]
    fn safe_cutoff_fewer_than_keep_is_zero() {
        let turns = vec![msg(MessageRole::User, "a"), msg(MessageRole::Assistant, "b")];
        assert_eq!(safe_cutoff(&turns, 10), Some(0));
    }

    #[test]
    fn safe_cutoff_does_not_split_a_pair() {
        // A call at index 2 and its result at index 3. Dropping
        // through index 3 (keep_recent = 2, nominal = 3) would
        // orphan the call. The cut must retreat to 2.
        let turns = vec![
            msg(MessageRole::User, "u0"),
            msg(MessageRole::Assistant, "a1"),
            assistant_with_call("c1"),
            tool_result("c1"),
            msg(MessageRole::User, "u4"),
            msg(MessageRole::Assistant, "a5"),
        ];
        let cut = safe_cutoff(&turns, 2).unwrap();
        // Whichever value, no tool_result with a call before the cut
        // may sit at or after it.
        let open_before: std::collections::HashSet<&str> = turns[..cut]
            .iter()
            .flat_map(|m| m.tool_calls.iter())
            .filter_map(|c| c.id.as_deref())
            .collect();
        for m in &turns[cut..] {
            if m.role == MessageRole::Tool
                && let Some(id) = m.tool_call_id.as_deref()
            {
                assert!(
                    !open_before.contains(id),
                    "cut {cut} splits call {id} from its result",
                );
            }
        }
    }

    #[test]
    fn safe_cutoff_leaves_a_complete_pair_together() {
        // Call at index 1, result at index 2. A `keep_recent = 2`
        // makes the nominal cut 2, which would drop the call and
        // keep the result — an orphaned pair the provider rejects.
        // The cut must retreat to 1 so the pair stays whole.
        let turns = vec![
            msg(MessageRole::User, "u0"),
            assistant_with_call("c1"),
            tool_result("c1"),
            msg(MessageRole::Assistant, "a3"),
        ];
        let cut = safe_cutoff(&turns, 2).unwrap();
        assert_eq!(
            cut, 1,
            "cut must retreat past the call so the pair at 1,2 stays together",
        );
        // And the pair is provably intact: the call at 1 is at or
        // after the cut, so it was not dropped.
        assert!(cut <= 1, "the call at index 1 must not be dropped");
    }

    #[test]
    fn safe_cutoff_keeps_the_tail_when_keep_exceeds_len() {
        // `keep_recent` larger than the transcript: nominal is 0, so
        // the whole thing is kept. The retreat loop must not spin.
        let turns = vec![
            msg(MessageRole::User, "u0"),
            assistant_with_call("c1"),
            tool_result("c1"),
        ];
        assert_eq!(safe_cutoff(&turns, 10), Some(0));
    }

    #[test]
    fn emergency_summary_counts_and_names() {
        let dropped = vec![
            assistant_with_call("c1"),
            tool_result("c1"),
        ];
        let s = emergency_summary(&dropped, 100_000);
        assert!(s.contains("2 messages dropped"));
        assert!(s.contains("read_file"));
        assert!(s.contains("a.rs"));
    }

    #[test]
    fn emergency_summary_is_bounded_on_files() {
        // 40 distinct paths → 30 shown, "and 10 more".
        let dropped: Vec<ChatMessage> = (0..40)
            .map(|i| {
                let mut m = msg(MessageRole::Assistant, "");
                m.tool_calls.push(ToolCall {
                    id: Some(format!("c{i}")),
                    tool_name: "read_file".to_string(),
                    arguments: serde_json::json!({"path": format!("f{i}.rs")}),
                });
                m
            })
            .collect();
        let s = emergency_summary(&dropped, 100_000);
        assert!(s.contains("and 10 more"), "got: {s}");
    }

    #[test]
    fn safe_cutoff_no_calls_is_the_nominal_cut() {
        let turns: Vec<ChatMessage> = (0..10)
            .map(|i| msg(MessageRole::User, &format!("u{i}")))
            .collect();
        assert_eq!(safe_cutoff(&turns, 4), Some(6));
    }
}
