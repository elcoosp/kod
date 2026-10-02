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

/// Fraction at which the current turn compacts before it runs. The
/// summary call has no time to return, so the emergency path is used
/// unless a background summary is already available.
pub const HARD_THRESHOLD: f64 = 0.95;

/// Floor on the lead band, in tokens. A window smaller than
/// `MIN_LEAD_BAND * 160 / 19` arms at `HARD_THRESHOLD * window -
/// MIN_LEAD_BAND`, i.e. earlier than the raw formula would suggest.
/// The floor exists so a tiny window does not arm a summary so early
/// that it is stale by the time the threshold is crossed.
pub const MIN_LEAD_BAND: u64 = 8_192;

/// Cap on the lead band, in tokens. The band is how much of the
/// transcript the armed summary misses before the hard threshold
/// triggers apply; the cap bounds that miss so a very large window
/// does not start the summary hundreds of thousands of tokens before
/// it is useful.
pub const MAX_LEAD_BAND: u64 = 32_000;

/// The lead band's width, in tokens, for a given window (delta §4.2).
///
/// `clamp(0.125 * HARD_THRESHOLD * window, MIN_LEAD_BAND,
/// MAX_LEAD_BAND)` — the design's formula, with `threshold` bound to
/// the hard threshold because that is the point the lead is *from*.
/// `0.125 * 0.95 = 0.11875 = 19/160`, so the arithmetic is exact in
/// integers.
pub fn lead_band_tokens(window_tokens: u64) -> u64 {
    (window_tokens.saturating_mul(19) / 160).clamp(MIN_LEAD_BAND, MAX_LEAD_BAND)
}

/// The absolute token count at which the background summary arms
/// (delta §4.2): `HARD_THRESHOLD * window - lead_band_tokens(window)`.
/// Below this, do nothing. At or above it (and below the hard
/// threshold), start the background summary. At or above the hard
/// threshold, compact the current turn.
pub fn arm_threshold_tokens(window_tokens: u64) -> u64 {
    let hard = window_tokens.saturating_mul(95) / 100;
    hard.saturating_sub(lead_band_tokens(window_tokens))
}

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
    let hard = budget_tokens.saturating_mul(95) / 100;
    if used_tokens >= hard {
        return Action::CompactNow;
    }
    // Delta §4.2: the arm point scales with the window. The fixed
    // 0.80 of the pre-change code is replaced by `hard - lead_band`:
    // a small window arms earlier proportionally (floor dominates),
    // a large one later proportionally (cap dominates).
    if used_tokens >= arm_threshold_tokens(budget_tokens) {
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

    // F2e-6: O(n log n), not the pre-fix O(n²) retreat. For every
    // tool-call id, the earliest message that opens it (`open`) and
    // the earliest tool message that closes it (`close`) define a
    // *forbidden* range of cut points: any cut in `(open, close]`
    // drops the opener while keeping its result — the straddle the
    // loop searched for. Merge those ranges and take the largest cut
    // ≤ nominal outside them.
    use std::collections::HashMap;
    let mut open_first: HashMap<&str, usize> = HashMap::new();
    let mut close_first: HashMap<&str, usize> = HashMap::new();
    for (i, m) in turns.iter().enumerate() {
        for c in &m.tool_calls {
            if let Some(id) = c.id.as_deref() {
                open_first.entry(id).or_insert(i);
            }
        }
        if m.role == MessageRole::Tool
            && let Some(id) = m.tool_call_id.as_deref()
        {
            close_first.entry(id).or_insert(i);
        }
    }
    let mut forbidden: Vec<(usize, usize)> = Vec::new();
    for (id, &o) in &open_first {
        if let Some(&c) = close_first.get(id)
            && o < c
        {
            // cut in [o + 1, c] straddles this call.
            forbidden.push((o + 1, c));
        }
    }
    forbidden.sort_unstable();
    // Merge touching/overlapping ranges (integer cut points, so
    // [a,b]∪[b+1,c] is contiguous).
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (s, e) in forbidden {
        match merged.last_mut() {
            Some(last) if s <= last.1 + 1 => {
                if e > last.1 {
                    last.1 = e;
                }
            }
            _ => merged.push((s, e)),
        }
    }
    // Largest cut ≤ nominal outside every merged range.
    let mut cut = nominal;
    for (s, e) in merged.iter().rev() {
        if *s <= cut && cut <= *e {
            if *s == 0 {
                return Some(0);
            }
            cut = s - 1;
        }
    }
    Some(cut)
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
        let turns = vec![
            msg(MessageRole::User, "a"),
            msg(MessageRole::Assistant, "b"),
        ];
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
        let dropped = vec![assistant_with_call("c1"), tool_result("c1")];
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

    // ---- Delta 4.2 lead band --------------------------------------

    #[test]
    fn lead_band_scales_with_window_inside_its_bounds() {
        // 200k x 19/160 = 23_750, inside [8_192, 32_000].
        assert_eq!(lead_band_tokens(200_000), 23_750);
        // 32k x 19/160 = 3_800, clamped up to the floor.
        assert_eq!(lead_band_tokens(32_000), MIN_LEAD_BAND);
        // 1M x 19/160 = 118_750, clamped down to the cap.
        assert_eq!(lead_band_tokens(1_000_000), MAX_LEAD_BAND);
    }

    #[test]
    fn arm_threshold_is_hard_minus_lead() {
        // 200k: hard = 190_000, lead = 23_750, arm = 166_250.
        assert_eq!(arm_threshold_tokens(200_000), 166_250);
        // 32k: hard = 30_400, lead clamped to 8_192, arm = 22_208.
        assert_eq!(arm_threshold_tokens(32_000), 22_208);
        // 1M: hard = 950_000, lead capped at 32_000, arm = 918_000.
        assert_eq!(arm_threshold_tokens(1_000_000), 918_000);
    }

    #[test]
    fn decide_arms_in_the_lead_band_and_compacts_at_hard() {
        // 200k window.
        assert_eq!(decide(166_249, 200_000), Action::None);
        assert_eq!(decide(166_250, 200_000), Action::StartBackground);
        assert_eq!(decide(189_999, 200_000), Action::StartBackground);
        assert_eq!(decide(190_000, 200_000), Action::CompactNow);
    }

    #[test]
    fn decide_arms_earlier_on_small_windows() {
        // The floor pulls the arm point up (as a fraction of the
        // window) on a 32k context — this is the 'prevent tiny-window
        // churn' branch of the design.
        assert_eq!(decide(22_207, 32_000), Action::None);
        assert_eq!(decide(22_208, 32_000), Action::StartBackground);
    }

    #[test]
    fn decide_arms_later_on_very_large_windows() {
        // The cap keeps the lead bounded, so on a 1M context the arm
        // point is 91.8% rather than the 88.125% the raw formula
        // would give — the armed summary misses at most 32k tokens
        // before apply.
        assert_eq!(decide(917_999, 1_000_000), Action::None);
        assert_eq!(decide(918_000, 1_000_000), Action::StartBackground);
        assert_eq!(decide(950_000, 1_000_000), Action::CompactNow);
    }

    #[test]
    fn safe_cutoff_no_calls_is_the_nominal_cut() {
        let turns: Vec<ChatMessage> = (0..10)
            .map(|i| msg(MessageRole::User, &format!("u{i}")))
            .collect();
        assert_eq!(safe_cutoff(&turns, 4), Some(6));
    }
}
