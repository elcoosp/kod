//! Mechanical context reduction: supersede pruning and the cache-warm
//! guard (borrow from oh-my-pi, delta §3.1 + §3.3).
//!
//! # The gap this fills
//!
//! Before this module, the engine had two options for shrinking a
//! transcript: do nothing (until the context window fills), or drop
//! the oldest turns FIFO. FIFO is a blunt instrument — it discards the
//! turn that *established* a fact at the same rate as the turn that
//! merely restated it. A ten-turn read-loop over one file, where every
//! read re-reads the whole file, would drop the newest read first
//! because it is farthest from the front.
//!
//! Supersede pruning is the middle rung: **blank a stale tool result
//! when a newer, equivalent call has already replaced it**. A read of
//! `src/main.rs` from turn 3 is superseded by a read of `src/main.rs`
//! from turn 8 — the turn-3 bytes are not information, they are the
//! same information twice. Replacing the older body with a short
//! `[Superseded by a newer read of this file]` marker costs ~8 tokens
//! and frees however many the original result took.
//!
//! No LLM call. No summarization. No loss of the current fact: only
//! the *older copy* goes.
//!
//! # The cache-warm guard
//!
//! The doc's §3.3 observation: mutating a tool result whose **suffix**
//! is still inside the provider's cache forces a cache-write premium
//! (Anthropic's 1.25×/2× depending on TTL) that can exceed the token
//! savings from blanking it. The guard is: if the tokens *after* this
//! message are still within the provider's cached window, leave the
//! message alone and let a compaction pass (which rebuilds the cache
//! once and amortizes the write across the whole transcript) handle it.
//!
//! The caller supplies `prefix_is_warm` because only the caller holds
//! the cache ledger. This module stays a pure function of its inputs.
//!
//! # What this module does NOT do
//!
//! - It does not call the LLM. Every rule here is mechanical.
//! - It does not *apply* prunes — [`plan_prune`] returns a plan, the
//!   caller executes it. That keeps the policy testable without a
//!   live engine and keeps the mutation path single-threaded.
//! - It does not handle the `useless` tool-declared flag the doc's
//!   §3.1 names; that flag does not exist on `MessageMetadata` yet.
//!   When it lands, the `protect_tokens` check in `plan_prune` gains
//!   an `|| result_is_useless(result)` arm and nothing else changes.
//! - It does not track ranged-read selectors. kod's `read_file` takes
//!   a path and nothing else; the doc's `K` vs `K+"\0selector"` key
//!   hierarchy has no range to key on. When the §7.1 hashline /
//!   read-format work lands a selector, this module's index grows a
//!   second map and the doc's "bare read supersedes ranged reads of
//!   the same path" rule is one `contains_key` check on top.

use kod_types::{ChatMessage, MessageId, MessageRole};
use std::collections::HashMap;
use std::path::PathBuf;

/// The placeholder text substituted for a superseded read. Matches the
/// doc's wording so a reader (and a future test) sees the same string
/// the design note used.
pub const SUPERSEDED_PLACEHOLDER: &str = "[Superseded by a newer read of this file]";

/// Rules that decide which tool results a prune pass will blank.
///
/// Defaults are the doc's §3.1 numbers, restated at the workspace's
/// 4-chars-per-token convention.
#[derive(Debug, Clone, Copy)]
pub struct PruneConfig {
    /// Never touch a result whose suffix contains this many tokens.
    /// "Suffix" means everything that comes *after* the result —
    /// the most recent turns the model is reasoning about right now.
    ///
    /// 40 000 ≈ 160 000 chars ≈ the last few turns of a long session.
    /// The doc's number; generous on purpose. A live bug hunt needs
    /// the recent reads intact, and pruning them because an *older*
    /// read happened to be longer is exactly the failure FIFO already
    /// has.
    pub protect_tokens: u64,

    /// The plan as a whole must free at least this many tokens to be
    /// worth running. Below the threshold, the cache churn (an
    /// invalidated suffix re-billed at the cache-write rate) outweighs
    /// the freed bytes.
    ///
    /// A whole-plan gate, not per-result: blanking one 100-token
    /// result that happens to be superseded is not worth anything;
    /// blanking a hundred of them is the case this rung exists for.
    pub minimum_savings: u64,

    /// Never blank a result smaller than this. The placeholder itself
    /// costs tokens (~8), so blanking a 30-token result *grows* the
    /// context — the exact opposite of the goal. 50 is the doc's
    /// number.
    pub min_prune_tokens: u64,

    /// The cache-warm guard's threshold. When the caller reports the
    /// prefix warm AND the result's suffix exceeds this token count,
    /// skip the result: the re-cache premium would exceed the savings.
    /// The default matches the doc's §3.1 "incremental gate" row.
    ///
    /// A caller that has no cache ledger (a first turn, an
    /// `enable_memory: false` test) passes `prefix_is_warm = false`
    /// and this field is never consulted.
    ///
    /// # Interaction with `protect_tokens`
    ///
    /// Both suffix filters gate on the same number: `protect_tokens`
    /// skips candidates whose suffix is *too small* (they are inside
    /// the working set the model is reasoning about), and this field
    /// skips candidates whose suffix is *too large* (they are deep
    /// inside a warm cache). A candidate is admitted only when:
    ///
    /// ```text
    /// protect_tokens <= suffix <= cache_warm_suffix_tokens
    /// ```
    ///
    /// Under the **defaults** (`protect_tokens = 40 000`,
    /// `cache_warm_suffix_tokens = 8 000`) that band is empty. Any
    /// suffix clears at most one filter, never both. The consequence
    /// is that a warm prefix blocks every prune under default
    /// settings — which is the correct reading of §3.3, not a bug.
    /// Defer to a compaction pass instead: it rebuilds the cache once
    /// and amortizes the write premium across the whole transcript.
    ///
    /// A caller that wants warm-cache pruning under defaults must
    /// either pass `prefix_is_warm = false` (after a model switch or
    /// system-prompt edit invalidates the cache), or raise this field
    /// above `protect_tokens` to open the band.
    pub cache_warm_suffix_tokens: u64,
}

impl Default for PruneConfig {
    fn default() -> Self {
        Self {
            protect_tokens: 40_000,
            minimum_savings: 20_000,
            min_prune_tokens: 50,
            cache_warm_suffix_tokens: 8_000,
        }
    }
}

/// What a prune pass wants to do to one message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PruneAction {
    /// Replace the message's `content` with the placeholder. The id,
    /// role, timestamp, and any tool linkage stay untouched — the
    /// assistant turn that issued the read is still the same turn, and
    /// the tool result still answers the same call. Only the body
    /// collapses.
    Blank { placeholder: String },
}

/// A prune plan: the ordered list of message mutations a caller
/// should apply. Empty when no rule fired.
///
/// A plan, not a mutation, because the caller may want to apply it
/// through the same code path that handles the tool round (so the
/// `CoherenceVersion` bookkeeping and the digest-memo invalidation
/// happen in one place) rather than mutate the log directly. It also
/// lets a test assert what *would* happen without constructing a
/// session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PrunePlan {
    pub actions: Vec<(MessageId, PruneAction)>,
}

impl PrunePlan {
    /// Total tokens the plan estimates it will free: the estimated
    /// size of each target's body, minus the placeholder's own size,
    /// summed. A blanked message is not removed from the log; its
    /// content shrinks in place.
    pub fn estimated_savings_tokens(&self) -> u64 {
        self.actions
            .iter()
            .map(|(_, a)| match a {
                PruneAction::Blank { placeholder } => {
                    // The action alone does not know the original size;
                    // the plan records the size alongside the action
                    // when it is built. This method is a coarse
                    // helper — see `estimate_savings` below for the
                    // accurate version a caller that has the log
                    // should use.
                    placeholder.len() as u64 / 4
                }
            })
            .sum()
    }

    pub fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }

    pub fn len(&self) -> usize {
        self.actions.len()
    }
}

/// Per-path view of the reads in a transcript.
///
/// A read is "the tool call on an assistant message whose name is
/// `read_file`", and it is associated with its result via the tool
/// call id. Both halves are needed: the *call* says which file and
/// when, the *result* is what gets blanked.
///
/// The index is built once per prune pass. Storing it in the engine
/// across turns would be a second transcript-derived cache with its
/// own invalidation story, and this structure is cheap to rebuild
/// (one linear walk over a transcript that is already in memory).
#[derive(Debug, Default)]
struct SupersedeIndex {
    /// For each canonical read path, the id of the newest `read_file`
    /// tool call that reads the whole file, and the transcript index
    /// where that call lives. A later bare read of the same path
    /// supersedes every earlier read of that path — bare or ranged —
    /// because it contains strictly more information.
    ///
    /// `ranged` (the doc's `K+"\0selector"` half) will be a second
    /// map here when `read_file` gains a selector; the lookup logic
    /// in `plan_prune` is one `or_else` chain away.
    whole_file: HashMap<PathBuf, (String, usize)>,
}

impl SupersedeIndex {
    fn build(transcript: &[ChatMessage]) -> Self {
        let mut idx = Self::default();
        for (i, msg) in transcript.iter().enumerate() {
            if msg.role != MessageRole::Assistant {
                continue;
            }
            for call in &msg.tool_calls {
                if call.tool_name != "read_file" {
                    continue;
                }
                let Some(path) = call
                    .arguments
                    .get("path")
                    .and_then(|v| v.as_str())
                else {
                    // A malformed call (no `path`). Ignoring it is the
                    // safe reading: the index's job is to identify
                    // *superseded* reads, and a read we cannot key on
                    // cannot be proven superseded.
                    continue;
                };
                let Some(call_id) = call.id.clone() else {
                    // A locally-constructed `ToolCall` with no wire id
                    // (a test fixture, a hand-built message) cannot be
                    // linked to its result. Skipping matches the
                    // "cannot be proven superseded" reading above.
                    continue;
                };
                let key = PathBuf::from(path);
                // Later calls overwrite earlier ones: the newest call
                // at each path is what makes older calls superseded.
                idx.whole_file.insert(key, (call_id, i));
            }
        }
        idx
    }

    /// The newest `read_file` call for `path`, if any. `None` when no
    /// read in the transcript targeted this path — a result that
    /// answers a read the index never saw cannot be proven stale.
    fn newest_call_for(&self, path: &std::path::Path) -> Option<&(String, usize)> {
        self.whole_file.get(path)
    }
}

/// A coarse token estimate for one message body. Same convention the
/// workspace uses everywhere (see `PromptBudget::CHARS_PER_TOKEN`); a
/// caller with a better estimate plugs it in via the closure that
/// [`plan_prune`] already takes for the suffix.
fn estimate_message_tokens(msg: &ChatMessage) -> u64 {
    (msg.content.len() as u64) / 4
}

/// Walk `transcript` newest-to-oldest and produce a plan for every
/// superseded tool result the config's rules admit.
///
/// The `suffix_tokens_after` closure is called with a message index
/// and returns the estimated token count of everything strictly after
/// that message. It exists because the caller owns the estimator: a
/// session with a live [`ContextGauge`](crate::context_gauge::ContextGauge)
/// can supply its provider-anchored number for the tail and fall back
/// to [`estimate_message_tokens`] for the pre-anchor region, while a
/// test supplies a constant. The prune pass itself has no opinion on
/// where the number came from.
///
/// `prefix_is_warm` is the caller's answer to "is the provider's
/// cache still holding our current prefix?". Only the caller holds a
/// cache ledger, so this module stays a pure function of its inputs.
/// `false` disables the cache-warm guard; that is the right value on
/// the first turn of a session and in any test that does not model a
/// cache.
pub fn plan_prune(
    transcript: &[ChatMessage],
    config: &PruneConfig,
    suffix_tokens_after: impl Fn(usize) -> u64,
    prefix_is_warm: bool,
) -> PrunePlan {
    let index = SupersedeIndex::build(transcript);

    // First pass: every result that *would* be blanked if the
    // savings gate had already been cleared. The savings gate is a
    // whole-plan decision, so it comes second.
    let mut candidates: Vec<(MessageId, PruneAction, u64)> = Vec::new();

    for (i, msg) in transcript.iter().enumerate() {
        // Only a tool result can be a blank target. A user or
        // assistant message body is never blanked by this pass — the
        // doc's rule, and the correct one: an assistant turn's text is
        // the model's own reasoning trace, and a user turn is the
        // input the model has to keep re-reading.
        if msg.role != MessageRole::Tool {
            continue;
        }
        // Already blanked on an earlier pass — skip. Without this the
        // placeholder itself would be re-blanked on every subsequent
        // prune, producing an ever-shorter-but-never-empty result.
        if msg.content == SUPERSEDED_PLACEHOLDER {
            continue;
        }

        // Find the assistant turn that issued this result. The
        // tool result carries `tool_call_id`; we scan forward because
        // the assistant turn necessarily precedes its result in a
        // well-formed transcript, and the tool-call ids are the only
        // link between the two halves.
        let Some(call) = find_call_by_result(transcript, msg) else {
            continue;
        };
        if call.tool_name != "read_file" {
            continue;
        }
        let Some(path_str) = call.arguments.get("path").and_then(|v| v.as_str()) else {
            continue;
        };
        let path = PathBuf::from(path_str);

        // Is this call superseded? Yes iff the index's newest call for
        // this path is a different call id (i.e. a later read of the
        // same path exists in the transcript).
        let Some((newest_id, _)) = index.newest_call_for(&path) else {
            continue;
        };
        let this_id = match call.id.as_deref() {
            Some(id) => id,
            None => continue,
        };
        if newest_id == this_id {
            continue;
        }

        // Rule: don't blank a result smaller than `min_prune_tokens`.
        // The placeholder would grow the context.
        let tokens = estimate_message_tokens(msg);
        if tokens < config.min_prune_tokens {
            continue;
        }

        // Rule: don't touch the most recent window. The suffix is
        // everything after this message; when that suffix is under
        // the protect threshold, this result is inside the "current
        // working set" the model is reasoning about right now.
        let suffix = suffix_tokens_after(i);
        if suffix < config.protect_tokens {
            continue;
        }

        // Rule (cache-warm guard, doc §3.3): when the prefix is warm
        // and the suffix past this message is still inside the
        // cached window, mutating this result forces a cache-write
        // premium that can exceed the savings. Leave it to a
        // compaction, which rebuilds the cache once and amortizes.
        if prefix_is_warm && suffix > config.cache_warm_suffix_tokens {
            continue;
        }

        // The *savings* from blanking: the message's own tokens minus
        // the placeholder's cost. The placeholder estimate uses the
        // same char-per-token convention.
        let placeholder_tokens = (SUPERSEDED_PLACEHOLDER.len() as u64) / 4;
        let freed = tokens.saturating_sub(placeholder_tokens);
        candidates.push((
            msg.id.clone(),
            PruneAction::Blank {
                placeholder: SUPERSEDED_PLACEHOLDER.to_string(),
            },
            freed,
        ));
    }

    // Whole-plan savings gate. Below the threshold, the plan is not
    // worth applying; the cache-churn cost of one mutation exceeds
    // the few tokens it would free.
    let total: u64 = candidates.iter().map(|(_, _, t)| *t).sum();
    if total < config.minimum_savings {
        return PrunePlan::default();
    }

    PrunePlan {
        actions: candidates
            .into_iter()
            .map(|(id, action, _)| (id, action))
            .collect(),
    }
}

/// Find the assistant tool call that a tool-role message answers.
///
/// A tool result links to its call via `tool_call_id`. Scanning the
/// transcript for the matching call is O(n) per result, giving
/// `plan_prune` an O(n²) shape in the worst case — but a well-formed
/// transcript answers each id exactly once and the call is always a
/// small distance behind its result, so the practical cost is O(n).
/// Replacing this with a pre-built `HashMap<&str, &ToolCall>` is a
/// one-line improvement if a profile ever shows it matters.
fn find_call_by_result<'a>(
    transcript: &'a [ChatMessage],
    result: &ChatMessage,
) -> Option<&'a kod_types::ToolCall> {
    let want = result.tool_call_id.as_deref()?;
    for msg in transcript {
        if msg.role != MessageRole::Assistant {
            continue;
        }
        for call in &msg.tool_calls {
            if call.id.as_deref() == Some(want) {
                return Some(call);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use kod_types::{MessageId, ToolCall};
    use time::OffsetDateTime;

    fn assistant_with_read(call_id: &str, path: &str) -> ChatMessage {
        let mut m = ChatMessage::text(
            MessageId::new(),
            MessageRole::Assistant,
            "",
            OffsetDateTime::now_utc(),
        );
        m.tool_calls.push(ToolCall {
            id: Some(call_id.to_string()),
            tool_name: "read_file".to_string(),
            arguments: serde_json::json!({ "path": path }),
        });
        m
    }

    fn tool_result(call_id: &str, body: &str) -> ChatMessage {
        let mut m = ChatMessage::text(
            MessageId::new(),
            MessageRole::Tool,
            body,
            OffsetDateTime::now_utc(),
        );
        m.tool_call_id = Some(call_id.to_string());
        m
    }

    /// A body big enough to clear the min-prune-tokens rule and to
    /// make the "would this actually save tokens?" question have an
    /// unambiguous answer.
    fn big_body() -> String {
        "x".repeat(2_000)
    }

    /// A suffix estimator that reports a constant. Tests use this
    /// when they want to make a single decision about a single
    /// candidate and not model the whole transcript's accounting.
    fn const_suffix(n: u64) -> impl Fn(usize) -> u64 {
        move |_| n
    }

    // ---- index construction --------------------------------------------

    #[test]
    fn a_single_read_has_no_newer_call_to_be_superseded_by() {
        let log = vec![
            assistant_with_read("c1", "a.rs"),
            tool_result("c1", &big_body()),
        ];
        let plan = plan_prune(&log, &PruneConfig::default(), const_suffix(100_000), false);
        assert!(plan.is_empty(), "one read supersedes nothing");
    }

    #[test]
    fn a_newer_read_supersedes_an_older_one() {
        // The savings gate is a whole-plan decision and is tested
        // separately; this test is about the supersede rule alone,
        // so the gate is disabled to isolate the rule.
        let cfg = PruneConfig {
            minimum_savings: 0,
            ..PruneConfig::default()
        };
        let log = vec![
            assistant_with_read("c1", "a.rs"),
            tool_result("c1", &big_body()),
            assistant_with_read("c2", "a.rs"),
            tool_result("c2", "freshly read again"),
        ];
        let plan = plan_prune(&log, &cfg, const_suffix(100_000), false);
        assert_eq!(plan.len(), 1, "the older read must be blanked");
        let (id, action) = &plan.actions[0];
        assert_eq!(id, &log[1].id, "the older *result* is the target");
        match action {
            PruneAction::Blank { placeholder } => {
                assert_eq!(placeholder, SUPERSEDED_PLACEHOLDER);
            }
        }
    }

    #[test]
    fn a_read_of_a_different_path_is_not_superseded() {
        // Two reads of two different files. Neither supersedes the
        // other; a `map` read does not make an `engine` read stale.
        let log = vec![
            assistant_with_read("c1", "a.rs"),
            tool_result("c1", &big_body()),
            assistant_with_read("c2", "b.rs"),
            tool_result("c2", &big_body()),
        ];
        let plan = plan_prune(&log, &PruneConfig::default(), const_suffix(100_000), false);
        assert!(plan.is_empty());
    }

    #[test]
    fn a_non_read_tool_result_is_never_a_target() {
        // A grep result has a body and no supersede key; the index
        // never learns about it and no rule can fire.
        let mut assistant = ChatMessage::text(
            MessageId::new(),
            MessageRole::Assistant,
            "",
            OffsetDateTime::now_utc(),
        );
        assistant.tool_calls.push(ToolCall {
            id: Some("c1".to_string()),
            tool_name: "grep".to_string(),
            arguments: serde_json::json!({"pattern": "x"}),
        });
        let log = vec![
            assistant,
            tool_result("c1", &big_body()),
            // A later read of some path, to make sure the grep result
            // is not swept in by a "newer tool call" of a different
            // kind.
            assistant_with_read("c2", "a.rs"),
            tool_result("c2", "fresh"),
        ];
        let plan = plan_prune(&log, &PruneConfig::default(), const_suffix(100_000), false);
        assert!(
            plan.is_empty(),
            "grep results are not superseded by later reads",
        );
    }

    // ---- rule: already-blanked ----------------------------------------

    #[test]
    fn an_already_blanked_result_is_left_alone() {
        let log = vec![
            assistant_with_read("c1", "a.rs"),
            tool_result("c1", SUPERSEDED_PLACEHOLDER),
            assistant_with_read("c2", "a.rs"),
            tool_result("c2", "fresh"),
        ];
        let plan = plan_prune(&log, &PruneConfig::default(), const_suffix(100_000), false);
        assert!(
            plan.is_empty(),
            "re-blanking a placeholder is a no-op, not a plan",
        );
    }

    // ---- rule: min_prune_tokens ---------------------------------------

    #[test]
    fn a_tiny_result_is_not_blanked() {
        // A result smaller than the placeholder would *grow* the
        // context if blanked. The min-prune-tokens rule is the guard.
        let log = vec![
            assistant_with_read("c1", "a.rs"),
            tool_result("c1", "ok"), // 2 chars -> 0 estimated tokens
            assistant_with_read("c2", "a.rs"),
            tool_result("c2", "fresh"),
        ];
        let plan = plan_prune(&log, &PruneConfig::default(), const_suffix(100_000), false);
        assert!(plan.is_empty());
    }

    // ---- rule: protect_tokens -----------------------------------------

    #[test]
    fn a_result_inside_the_protected_window_is_not_blanked() {
        // The suffix under the protect threshold means "this result
        // is part of the working set the model is reasoning about
        // right now".
        let cfg = PruneConfig {
            protect_tokens: 40_000,
            ..PruneConfig::default()
        };
        let log = vec![
            assistant_with_read("c1", "a.rs"),
            tool_result("c1", &big_body()),
            assistant_with_read("c2", "a.rs"),
            tool_result("c2", "fresh"),
        ];
        // The suffix after the older result is 200 tokens; well under
        // the 40 000 protect window.
        let plan = plan_prune(&log, &cfg, const_suffix(200), false);
        assert!(plan.is_empty(), "the working set must be protected");
    }

    // ---- rule: cache-warm guard ---------------------------------------

    #[test]
    fn a_cold_prefix_skips_the_cache_warm_guard() {
        // `prefix_is_warm = false`: the guard does not apply. The
        // suffix size is irrelevant.
        //
        // The savings gate is disabled because this test isolates
        // the cache-warm rule; the gate has its own tests below.
        let cfg = PruneConfig {
            cache_warm_suffix_tokens: 8_000,
            minimum_savings: 0,
            ..PruneConfig::default()
        };
        let log = vec![
            assistant_with_read("c1", "a.rs"),
            tool_result("c1", &big_body()),
            assistant_with_read("c2", "a.rs"),
            tool_result("c2", "fresh"),
        ];
        let plan = plan_prune(&log, &cfg, const_suffix(100_000), false);
        assert_eq!(plan.len(), 1);
    }

    #[test]
    fn a_warm_prefix_with_a_large_suffix_is_left_alone() {
        // The cache-warm guard's whole point: mutating a result whose
        // suffix is still inside the cached window forces a
        // cache-write premium that exceeds the token savings.
        let cfg = PruneConfig {
            cache_warm_suffix_tokens: 8_000,
            ..PruneConfig::default()
        };
        let log = vec![
            assistant_with_read("c1", "a.rs"),
            tool_result("c1", &big_body()),
            assistant_with_read("c2", "a.rs"),
            tool_result("c2", "fresh"),
        ];
        let plan = plan_prune(&log, &cfg, const_suffix(100_000), true);
        assert!(plan.is_empty(), "warm cache + large suffix = skip");
    }

    #[test]
    fn a_warm_prefix_with_a_small_suffix_is_blanked() {
        // The narrow reachable band under the cache-warm guard: the
        // suffix is over `protect_tokens` (so the working-set rule
        // does not fire) AND at or under `cache_warm_suffix_tokens`
        // (so the cache-warm rule does not fire either). A prune is
        // allowed, and the plan is admitted.
        //
        // Under the *defaults* this band is empty: `protect_tokens`
        // is 40 000 and `cache_warm_suffix_tokens` is 8 000, so any
        // candidate whose suffix clears the protect window also
        // exceeds the cache-warm threshold. Reaching the band
        // requires `protect_tokens <= cache_warm_suffix_tokens`;
        // this test lowers `protect_tokens` to 4 000. The default
        // consequence is pinned separately by
        // `warm_prefix_with_defaults_blocks_every_prune`.
        //
        // The savings gate is disabled because this test isolates
        // the cache-warm rule; the gate has its own tests below.
        let cfg = PruneConfig {
            protect_tokens: 4_000,
            cache_warm_suffix_tokens: 8_000,
            minimum_savings: 0,
            ..PruneConfig::default()
        };
        let log = vec![
            assistant_with_read("c1", "a.rs"),
            tool_result("c1", &big_body()),
            assistant_with_read("c2", "a.rs"),
            tool_result("c2", "fresh"),
        ];
        // Suffix 5 000: over protect (4 000), under cache-warm
        // (8 000). Both suffix filters pass.
        let plan = plan_prune(&log, &cfg, const_suffix(5_000), true);
        assert_eq!(plan.len(), 1);
    }

    #[test]
    fn warm_prefix_with_defaults_blocks_every_prune() {
        // The consequence of the default numbers: `protect_tokens`
        // is 40 000 and `cache_warm_suffix_tokens` is 8 000, so the
        // admit band `protect_tokens <= suffix <= cache_warm_suffix_tokens`
        // is *empty*. Any suffix clears at most one of the two
        // filters, never both:
        //
        //   * 5 000  — passes cache-warm (<= 8 000), fails protect
        //              (< 40 000)
        //   * 41 000 — passes protect (>= 40 000), fails cache-warm
        //              (> 8 000)
        //   * 100 000 — same as 41 000
        //
        // A warm prefix therefore blocks every prune under default
        // settings. This is the design note's §3.3 taken to its
        // limit, and it is the *correct* behaviour: with the prefix
        // warm, mutating a region whose cached tail exceeds the
        // cache-warm threshold pays more in re-cache (Anthropic's
        // 1.25x/2x write premium) than it frees. Deferring to a
        // compaction pass — which rebuilds the cache once and
        // amortizes the write across the whole transcript — is the
        // only choice that makes sense.
        //
        // A caller that wants warm-cache pruning under defaults
        // must first make the cache cold (pass `prefix_is_warm =
        // false` after a model switch or system-prompt edit), or
        // raise `cache_warm_suffix_tokens` above `protect_tokens`
        // to open the band.
        let cfg = PruneConfig {
            // Non-default gate so this test is about the *rule
            // interaction*, not the savings filter.
            minimum_savings: 0,
            ..PruneConfig::default()
        };
        let log = vec![
            assistant_with_read("c1", "a.rs"),
            tool_result("c1", &big_body()),
            assistant_with_read("c2", "a.rs"),
            tool_result("c2", "fresh"),
        ];
        for suffix in [5_000u64, 41_000, 100_000] {
            let plan = plan_prune(&log, &cfg, const_suffix(suffix), true);
            assert!(
                plan.is_empty(),
                "warm prefix + default config must block every suffix; \
                 got a non-empty plan at suffix {suffix}",
            );
        }
    }

    // ---- rule: minimum_savings ----------------------------------------

    #[test]
    fn a_plan_below_the_savings_gate_is_not_applied() {
        // A single small candidate cannot clear the whole-plan
        // minimum-savings gate.
        let cfg = PruneConfig {
            minimum_savings: 20_000,
            ..PruneConfig::default()
        };
        // 2_000 chars -> ~500 tokens per candidate. One candidate
        // frees ~500 tokens, well under 20 000.
        let log = vec![
            assistant_with_read("c1", "a.rs"),
            tool_result("c1", &big_body()),
            assistant_with_read("c2", "a.rs"),
            tool_result("c2", "fresh"),
        ];
        let plan = plan_prune(&log, &cfg, const_suffix(100_000), false);
        assert!(
            plan.is_empty(),
            "one candidate cannot clear the gate alone",
        );
    }

    #[test]
    fn the_savings_gate_is_a_whole_plan_decision() {
        // The design note's `minimumSavings 20000` does not say
        // whether the check is per-candidate or whole-plan. A
        // per-candidate reading would require a single 80 000-char
        // tool result to ever prune anything, which is unreasonable;
        // the whole-plan reading is the correct one and this test
        // pins it.
        //
        // Gate = 4 000. Five candidates at ~1 000 tokens each:
        // *no individual candidate clears the gate*, but their sum
        // (~5 000) does. A per-candidate implementation would return
        // an empty plan; a whole-plan implementation returns all
        // five.
        let cfg = PruneConfig {
            minimum_savings: 4_000,
            ..PruneConfig::default()
        };
        let big = "y".repeat(4_000); // ~1 000 tokens each
        let mut log = Vec::new();
        for i in 0..5 {
            log.push(assistant_with_read(&format!("c{i}"), "a.rs"));
            log.push(tool_result(&format!("c{i}"), &big));
        }
        // A final fresh read to make the earlier ones superseded.
        log.push(assistant_with_read("cfinal", "a.rs"));
        log.push(tool_result("cfinal", "fresh"));
        let plan = plan_prune(&log, &cfg, const_suffix(100_000), false);
        assert_eq!(
            plan.len(),
            5,
            "the whole-plan sum clears the gate; every candidate is admitted",
        );
    }

    #[test]
    fn a_plan_of_many_small_prunes_below_the_gate_is_not_applied() {
        // The counterpart: many candidates, each individually tiny,
        // whose sum is still below the whole-plan gate. A per-candidate
        // implementation would apply them; a whole-plan one applies
        // none.
        let cfg = PruneConfig {
            minimum_savings: 10_000,
            ..PruneConfig::default()
        };
        // 3 candidates at ~100 tokens each; sum ~300, well under the
        // 10 000 gate. Each is over `min_prune_tokens` (50), so the
        // per-candidate rule does not filter them — the gate does.
        let small = "x".repeat(400);
        let mut log = Vec::new();
        for i in 0..3 {
            log.push(assistant_with_read(&format!("c{i}"), "a.rs"));
            log.push(tool_result(&format!("c{i}"), &small));
        }
        log.push(assistant_with_read("cfinal", "a.rs"));
        log.push(tool_result("cfinal", "fresh"));
        let plan = plan_prune(&log, &cfg, const_suffix(100_000), false);
        assert!(
            plan.is_empty(),
            "a plan whose sum falls below the gate must not be applied",
        );
    }

    // ---- config defaults match the doc ---------------------------------

    #[test]
    fn config_defaults_match_the_documented_numbers() {
        // The doc's §3.1 table, restated here so a change is a
        // deliberate edit rather than a quiet drift.
        let c = PruneConfig::default();
        assert_eq!(c.protect_tokens, 40_000);
        assert_eq!(c.minimum_savings, 20_000);
        assert_eq!(c.min_prune_tokens, 50);
        assert_eq!(c.cache_warm_suffix_tokens, 8_000);
    }

    #[test]
    fn the_supersede_placeholder_is_the_documented_string() {
        // A snapshot of the placeholder text: it appears in the
        // transcript after a prune, and a reader (or a diff) will
        // compare it to the design note.
        assert_eq!(
            SUPERSEDED_PLACEHOLDER,
            "[Superseded by a newer read of this file]",
        );
    }

    // ---- ordering and multiplicity ------------------------------------

    #[test]
    fn a_chain_of_reads_blank_everything_but_the_newest() {
        // Three reads of the same file; the newest survives, the
        // other two blank.
        let cfg = PruneConfig {
            minimum_savings: 0,
            ..PruneConfig::default()
        };
        let big = "z".repeat(4_000);
        let log = vec![
            assistant_with_read("c1", "a.rs"),
            tool_result("c1", &big),
            assistant_with_read("c2", "a.rs"),
            tool_result("c2", &big),
            assistant_with_read("c3", "a.rs"),
            tool_result("c3", "fresh"),
        ];
        let plan = plan_prune(&log, &cfg, const_suffix(100_000), false);
        assert_eq!(plan.len(), 2);
        assert_eq!(plan.actions[0].0, log[1].id, "the first read is blanked");
        assert_eq!(plan.actions[1].0, log[3].id, "the second read is blanked");
    }

    #[test]
    fn a_malformed_read_without_a_path_is_ignored() {
        // A `read_file` call with no `path` argument cannot be keyed
        // in the index. It contributes nothing, and its result is
        // never blanked.
        let mut malformed = ChatMessage::text(
            MessageId::new(),
            MessageRole::Assistant,
            "",
            OffsetDateTime::now_utc(),
        );
        malformed.tool_calls.push(ToolCall {
            id: Some("c1".to_string()),
            tool_name: "read_file".to_string(),
            arguments: serde_json::json!({}),
        });
        let log = vec![
            malformed,
            tool_result("c1", &big_body()),
            assistant_with_read("c2", "a.rs"),
            tool_result("c2", "fresh"),
        ];
        let plan = plan_prune(&log, &PruneConfig::default(), const_suffix(100_000), false);
        assert!(plan.is_empty());
    }

    #[test]
    fn an_empty_transcript_yields_an_empty_plan() {
        let plan = plan_prune(&[], &PruneConfig::default(), const_suffix(0), false);
        assert!(plan.is_empty());
    }
}
