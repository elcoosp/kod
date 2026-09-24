//! Tool-call loop detection (borrow from oh-my-pi, delta §9.4).
//!
//! # The failure this catches
//!
//! A model can get stuck calling the same tool the same way. A
//! `read_file` of a path the tool keeps reporting as missing; a
//! `grep` whose regex never matches because of a typo the model
//! cannot see; a `list_files` on a directory whose contents it
//! cannot interpret. The loop consumes budget, produces nothing
//! useful, and looks to the user like a hung turn.
//!
//! # The fix
//!
//! A [`ToolLoopGuard`] sees each round of tool calls the agent
//! issues, hashes it, and — when the last `threshold` rounds carry
//! identical fingerprints — emits a [`Corrective`] the engine can
//! inject as a system message. The corrective names the tool, the
//! count, and short summaries of the arguments and result so the
//! model has a concrete reason to change course.
//!
//! # What "identical" means
//!
//! Two rounds match iff their tool calls are the same **as a set**,
//! with JSON object keys sorted and the top-level `intent` field
//! stripped:
//!
//! * Sorted keys — `{"a":1,"b":2}` and `{"b":2,"a":1}` are the same
//!   arguments. A model cannot defeat the detector by re-emitting
//!   JSON in a different key order.
//! * `intent` stripped — the registry injects a free-form `intent`
//!   string into every schema (`kod_tools::registry::
//!   inject_intent_field`). Re-issuing the same call with a
//!   different "why" is still looping; the field is descriptive
//!   metadata, not identity.
//! * Set of calls — `[read_file(a), read_file(b)]` and
//!   `[read_file(b), read_file(a)]` are the same round. Reordering
//!   parallel calls is not a behavior change.
//!
//! # What this is NOT
//!
//! * Not a semantic loop detector. Two `read_file` calls on
//!   *different* paths never match, however many times they are
//!   issued. That is deliberate — sometimes reading a hundred files
//!   in a row is the job.
//!
//! * Not a hard cap. The guard does not refuse a call. It emits a
//!   corrective and the engine decides what to do with it.
//!   Refusing calls is `kod_core::tool_quota`'s job.
//!
//! * Not a metrics collector. A caller that wants to log every
//!   fingerprint does so at the call site.
//!
//! # The other half
//!
//! The design note's §9.4 pairs this with an **unexpected-stop
//! classifier** (stopReason "stop" + text or signed thinking, no
//! tool call, one judge question, threshold 0.5). That half needs
//! the judgment framework from §9.5, which does not exist yet.
//! This module is self-contained and useful without it.

use kod_types::{ToolCall, ToolResult};
use serde_json::Value;
use std::collections::VecDeque;

/// Default threshold: how many consecutive identical rounds before a
/// corrective fires.
///
/// Three is the smallest value that is unambiguously a loop. Two
/// identical rounds can happen by accident (a retry, a pair the
/// model did not coalesce); three is a pattern.
pub const DEFAULT_LOOP_THRESHOLD: usize = 3;

/// Cap on the corrective's arguments summary. The doc's number.
pub const MAX_ARGUMENTS_SUMMARY_CHARS: usize = 400;

/// Cap on the corrective's result summary. The doc's number.
pub const MAX_RESULT_SUMMARY_CHARS: usize = 200;

/// A guard for one transcript.
#[derive(Debug, Clone)]
pub struct ToolLoopGuard {
    threshold: usize,
    /// Fingerprints of the most recent rounds, oldest first.
    /// Bounded to `threshold` entries.
    recent: VecDeque<u64>,
}

/// What the guard emits when a loop is detected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Corrective {
    /// Name of the repeated tool. When the round contains more than
    /// one call, this is the name of the first call in sorted order
    /// — a stable choice that names something concrete without
    /// list-building.
    pub tool_name: String,
    /// How many identical rounds were observed (== the threshold).
    pub count: u32,
    /// Short summary of the round's arguments, truncated to
    /// [`MAX_ARGUMENTS_SUMMARY_CHARS`].
    pub arguments_summary: String,
    /// Short summary of the round's results, truncated to
    /// [`MAX_RESULT_SUMMARY_CHARS`].
    pub result_summary: String,
}

impl Default for ToolLoopGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolLoopGuard {
    /// A guard with [`DEFAULT_LOOP_THRESHOLD`].
    pub fn new() -> Self {
        Self::with_threshold(DEFAULT_LOOP_THRESHOLD)
    }

    /// A guard with a custom threshold. Clamped to `>= 2`: a
    /// threshold of 1 would fire on the first round, which is
    /// never a loop.
    pub fn with_threshold(threshold: usize) -> Self {
        let threshold = threshold.max(2);
        Self {
            threshold,
            recent: VecDeque::with_capacity(threshold),
        }
    }

    /// The configured threshold.
    pub fn threshold(&self) -> usize {
        self.threshold
    }

    /// Rounds observed since the last emit or reset. For tests and a
    /// UI readout.
    pub fn rounds_seen(&self) -> usize {
        self.recent.len()
    }

    /// Forget the streak.
    pub fn reset(&mut self) {
        self.recent.clear();
    }

    /// Feed one round of tool calls and its results. Returns a
    /// [`Corrective`] when the last `threshold` rounds have been
    /// identical; `None` otherwise.
    ///
    /// The guard clears its streak on emit, so a model that keeps
    /// looping gets a fresh corrective every `threshold` rounds
    /// rather than one-and-done. A model that returns to text (an
    /// empty calls slice) resets the streak immediately — no
    /// corrective, because the loop is broken.
    pub fn observe_round(
        &mut self,
        calls: &[ToolCall],
        results: &[ToolResult],
    ) -> Option<Corrective> {
        if calls.is_empty() {
            self.recent.clear();
            return None;
        }

        let fp = fingerprint_round(calls);
        self.recent.push_back(fp);
        while self.recent.len() > self.threshold {
            self.recent.pop_front();
        }

        if self.recent.len() < self.threshold {
            return None;
        }
        let newest = *self.recent.back().expect("len >= threshold >= 2");
        if !self.recent.iter().all(|&f| f == newest) {
            return None;
        }

        let corrective =
            build_corrective(calls, results, self.threshold as u32);
        self.recent.clear();
        Some(corrective)
    }
}

// ---------------------------------------------------------------------------
// Fingerprinting
// ---------------------------------------------------------------------------

/// FNV-1a 64-bit offset basis and prime.
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Length-prefixed FNV-1a-64 feed. Length prefixing makes the
/// encoding injective: `("ab", "c")` and `("a", "bc")` produce
/// different digests. A NUL separator would not, since NUL is a
/// valid byte inside a JSON string.
fn feed(h: &mut u64, bytes: &[u8]) {
    for b in (bytes.len() as u64).to_le_bytes() {
        *h ^= b as u64;
        *h = h.wrapping_mul(FNV_PRIME);
    }
    for &b in bytes {
        *h ^= b as u64;
        *h = h.wrapping_mul(FNV_PRIME);
    }
}

/// Fingerprint one round: sort the calls by `(name, canonical_args)`,
/// hash each component with length prefixes.
///
/// The sort is what gives set semantics. `parts.sort()` on a `Vec` of
/// `(&str, String)` tuples sorts first by name, then lexically by
/// canonical args, which is stable across reorderings of the same
/// call set.
fn fingerprint_round(calls: &[ToolCall]) -> u64 {
    let mut parts: Vec<(&str, String)> = calls
        .iter()
        .map(|c| (c.tool_name.as_str(), canonical_args(&c.arguments)))
        .collect();
    parts.sort();

    let mut h = FNV_OFFSET;
    for (name, args) in parts {
        feed(&mut h, name.as_bytes());
        feed(&mut h, args.as_bytes());
    }
    h
}

/// Canonical JSON for a call's arguments, with the top-level `intent`
/// field stripped.
fn canonical_args(v: &Value) -> String {
    let stripped = strip_top_level_intent(v);
    let mut out = String::new();
    write_canon(&mut out, &stripped);
    out
}

/// Remove the top-level `intent` key from an object, if present.
///
/// Only top-level. Nested `intent` keys are left alone: the injected
/// field is top-level by construction (see `kod_tools::registry::
/// inject_intent_field`), and a nested `intent` would be the
/// caller's own data.
fn strip_top_level_intent(v: &Value) -> Value {
    match v {
        Value::Object(o) => {
            let mut o = o.clone();
            o.remove("intent");
            Value::Object(o)
        }
        _ => v.clone(),
    }
}

/// Canonical JSON: object keys sorted, no whitespace. Duplicated from
/// `transcript_coherence` (30 lines, stable, not worth a shared
/// module for two call sites with different pre-processing needs).
fn write_canon(out: &mut String, v: &Value) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => {
            out.push_str(&serde_json::to_string(s).unwrap_or_default());
        }
        Value::Array(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canon(out, x);
            }
            out.push(']');
        }
        Value::Object(o) => {
            let mut keys: Vec<&String> = o.keys().collect();
            keys.sort();
            out.push('{');
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(k.as_str()).unwrap_or_default());
                out.push(':');
                write_canon(out, &o[k.as_str()]);
            }
            out.push('}');
        }
    }
}

// ---------------------------------------------------------------------------
// Corrective construction
// ---------------------------------------------------------------------------

/// Build the corrective from the current (looping) round.
fn build_corrective(
    calls: &[ToolCall],
    results: &[ToolResult],
    count: u32,
) -> Corrective {
    // The first call in sorted order — stable, and names *something*
    // concrete. An empty-calls round cannot reach here (observe_round
    // returns early on `calls.is_empty()`), so `.first()` is safe;
    // the fallback is a string that will never appear, kept as
    // defensive code rather than `unwrap`.
    let mut names: Vec<&str> = calls.iter().map(|c| c.tool_name.as_str()).collect();
    names.sort();
    let tool_name = names.first().copied().unwrap_or("(none)").to_string();

    let mut args_raw = String::new();
    for (i, call) in calls.iter().enumerate() {
        if i > 0 {
            args_raw.push(' ');
        }
        let args = canonical_args(&call.arguments);
        args_raw.push_str(&call.tool_name);
        args_raw.push('(');
        args_raw.push_str(&args);
        args_raw.push(')');
    }
    let arguments_summary =
        kod_types::strutil::truncate_chars(&args_raw, MAX_ARGUMENTS_SUMMARY_CHARS)
            .to_string();

    let mut res_raw = String::new();
    for (i, result) in results.iter().enumerate() {
        if i > 0 {
            res_raw.push_str(" | ");
        }
        res_raw.push_str(&summarize_result(result));
    }
    let result_summary =
        kod_types::strutil::truncate_chars(&res_raw, MAX_RESULT_SUMMARY_CHARS)
            .to_string();

    Corrective {
        tool_name,
        count,
        arguments_summary,
        result_summary,
    }
}

/// One-line summary of a tool result. Compact JSON for a success;
/// a labelled error or confirmation otherwise.
fn summarize_result(result: &ToolResult) -> String {
    match result {
        ToolResult::Success(v) => {
            serde_json::to_string(v).unwrap_or_else(|_| "<unserializable>".to_string())
        }
        ToolResult::Error(s) => format!("Error: {s}"),
        ToolResult::RequiresConfirmation { description, .. } => {
            format!("Confirmation: {description}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(name: &str, args: Value) -> ToolCall {
        ToolCall {
            id: None,
            tool_name: name.to_string(),
            arguments: args,
        }
    }

    fn ok(v: Value) -> ToolResult {
        ToolResult::Success(v)
    }

    fn err(s: &str) -> ToolResult {
        ToolResult::Error(s.to_string())
    }

    // -----------------------------------------------------------------
    // Fingerprint
    // -----------------------------------------------------------------

    #[test]
    fn identical_rounds_fingerprint_the_same() {
        let a = vec![call("read_file", json!({"path": "x"}))];
        let b = vec![call("read_file", json!({"path": "x"}))];
        assert_eq!(fingerprint_round(&a), fingerprint_round(&b));
    }

    #[test]
    fn different_args_fingerprint_differently() {
        let a = vec![call("read_file", json!({"path": "x"}))];
        let b = vec![call("read_file", json!({"path": "y"}))];
        assert_ne!(fingerprint_round(&a), fingerprint_round(&b));
    }

    #[test]
    fn different_tool_names_fingerprint_differently() {
        let a = vec![call("read_file", json!({"path": "x"}))];
        let b = vec![call("grep", json!({"path": "x"}))];
        assert_ne!(fingerprint_round(&a), fingerprint_round(&b));
    }

    #[test]
    fn argument_key_order_does_not_matter() {
        let a = vec![call("read_file", json!({"path": "x", "encoding": "utf-8"}))];
        let b = vec![call("read_file", json!({"encoding": "utf-8", "path": "x"}))];
        assert_eq!(fingerprint_round(&a), fingerprint_round(&b));
    }

    #[test]
    fn nested_argument_key_order_does_not_matter() {
        let a = vec![call("tool", json!({"o": {"a": 1, "b": 2}}))];
        let b = vec![call("tool", json!({"o": {"b": 2, "a": 1}}))];
        assert_eq!(fingerprint_round(&a), fingerprint_round(&b));
    }

    #[test]
    fn call_order_within_a_round_does_not_matter() {
        let a = vec![
            call("read_file", json!({"path": "a"})),
            call("read_file", json!({"path": "b"})),
        ];
        let b = vec![
            call("read_file", json!({"path": "b"})),
            call("read_file", json!({"path": "a"})),
        ];
        assert_eq!(fingerprint_round(&a), fingerprint_round(&b));
    }

    #[test]
    fn top_level_intent_is_stripped() {
        // The registry injects `intent`; re-issuing the same call
        // with a different "why" is still looping.
        let a = vec![call(
            "read_file",
            json!({"path": "x", "intent": "check the config"}),
        )];
        let b = vec![call(
            "read_file",
            json!({"path": "x", "intent": "verify the schema"}),
        )];
        assert_eq!(fingerprint_round(&a), fingerprint_round(&b));
    }

    #[test]
    fn nested_intent_is_not_stripped() {
        // A nested `intent` is the caller's own data; stripping it
        // would silently collapse distinct calls.
        let a = vec![call("tool", json!({"outer": {"intent": "a"}}))];
        let b = vec![call("tool", json!({"outer": {"intent": "b"}}))];
        assert_ne!(fingerprint_round(&a), fingerprint_round(&b));
    }

    #[test]
    fn an_array_argument_order_matters() {
        // Arrays are ordered; a permuted array is a different value.
        let a = vec![call("tool", json!({"xs": [1, 2, 3]}))];
        let b = vec![call("tool", json!({"xs": [3, 2, 1]}))];
        assert_ne!(fingerprint_round(&a), fingerprint_round(&b));
    }

    // -----------------------------------------------------------------
    // Guard
    // -----------------------------------------------------------------

    #[test]
    fn a_fresh_guard_reports_no_loop() {
        let g = ToolLoopGuard::new();
        assert_eq!(g.threshold(), DEFAULT_LOOP_THRESHOLD);
        assert_eq!(g.rounds_seen(), 0);
    }

    #[test]
    fn threshold_is_clamped_to_at_least_two() {
        // A threshold of 1 would fire on the first round, which is
        // never a loop.
        assert_eq!(ToolLoopGuard::with_threshold(0).threshold(), 2);
        assert_eq!(ToolLoopGuard::with_threshold(1).threshold(), 2);
        assert_eq!(ToolLoopGuard::with_threshold(2).threshold(), 2);
    }

    #[test]
    fn two_identical_rounds_do_not_fire_at_threshold_three() {
        let mut g = ToolLoopGuard::new();
        let c = vec![call("read_file", json!({"path": "x"}))];
        let r = vec![ok(json!({}))];
        assert!(g.observe_round(&c, &r).is_none());
        assert!(g.observe_round(&c, &r).is_none());
        assert_eq!(g.rounds_seen(), 2);
    }

    #[test]
    fn three_identical_rounds_fire_and_emit_a_corrective() {
        let mut g = ToolLoopGuard::new();
        let c = vec![call("read_file", json!({"path": "x"}))];
        let r = vec![err("file not found")];
        let _ = g.observe_round(&c, &r);
        let _ = g.observe_round(&c, &r);
        let corrective = g.observe_round(&c, &r).expect("third round fires");
        assert_eq!(corrective.tool_name, "read_file");
        assert_eq!(corrective.count, 3);
        assert!(
            corrective.arguments_summary.contains("read_file"),
            "arguments summary must name the tool: {}",
            corrective.arguments_summary,
        );
        assert!(
            corrective.result_summary.contains("file not found"),
            "result summary must name the failure: {}",
            corrective.result_summary,
        );
        // Streak cleared after emit.
        assert_eq!(g.rounds_seen(), 0);
    }

    #[test]
    fn a_break_in_the_streak_prevents_the_emit() {
        let mut g = ToolLoopGuard::new();
        let c1 = vec![call("read_file", json!({"path": "x"}))];
        let c2 = vec![call("read_file", json!({"path": "y"}))];
        let r = vec![ok(json!({}))];
        let _ = g.observe_round(&c1, &r);
        let _ = g.observe_round(&c2, &r); // different
        // Window is [c1, c2]; adding c2 again gives [c1, c2, c2] —
        // not all equal, no emit.
        assert!(g.observe_round(&c2, &r).is_none());
        // Adding c2 a third time: push → [c2, c2, c2] after pop, emit.
        assert!(g.observe_round(&c2, &r).is_some());
    }

    #[test]
    fn an_empty_round_clears_the_streak() {
        let mut g = ToolLoopGuard::new();
        let c = vec![call("read_file", json!({"path": "x"}))];
        let r = vec![ok(json!({}))];
        let _ = g.observe_round(&c, &r);
        let _ = g.observe_round(&c, &r);
        assert_eq!(g.rounds_seen(), 2);
        // Model returns to text — no calls. Streak resets.
        assert!(g.observe_round(&[], &[]).is_none());
        assert_eq!(g.rounds_seen(), 0);
    }

    #[test]
    fn a_looping_model_gets_periodic_reminders() {
        // After an emit, the streak is cleared. A model that ignores
        // the corrective and keeps looping gets another one at
        // round 6, 9, ...
        let mut g = ToolLoopGuard::new();
        let c = vec![call("read_file", json!({"path": "x"}))];
        let r = vec![ok(json!({}))];
        assert!(g.observe_round(&c, &r).is_none());
        assert!(g.observe_round(&c, &r).is_none());
        assert!(g.observe_round(&c, &r).is_some());
        // Three more identical rounds → another emit.
        assert!(g.observe_round(&c, &r).is_none());
        assert!(g.observe_round(&c, &r).is_none());
        assert!(g.observe_round(&c, &r).is_some());
    }

    #[test]
    fn a_custom_threshold_is_honoured() {
        let mut g = ToolLoopGuard::with_threshold(4);
        let c = vec![call("read_file", json!({"path": "x"}))];
        let r = vec![ok(json!({}))];
        assert!(g.observe_round(&c, &r).is_none());
        assert!(g.observe_round(&c, &r).is_none());
        assert!(g.observe_round(&c, &r).is_none());
        assert!(g.observe_round(&c, &r).is_some(), "fourth round fires");
    }

    #[test]
    fn reset_clears_the_window() {
        let mut g = ToolLoopGuard::new();
        let c = vec![call("read_file", json!({"path": "x"}))];
        let r = vec![ok(json!({}))];
        let _ = g.observe_round(&c, &r);
        let _ = g.observe_round(&c, &r);
        assert_eq!(g.rounds_seen(), 2);
        g.reset();
        assert_eq!(g.rounds_seen(), 0);
    }

    // -----------------------------------------------------------------
    // Corrective shape
    // -----------------------------------------------------------------

    #[test]
    fn the_arguments_summary_is_capped() {
        // A round with many verbose calls: the summary must not
        // exceed the cap.
        let big_path = "x".repeat(200);
        let calls: Vec<ToolCall> = (0..10)
            .map(|i| {
                call(
                    "read_file",
                    json!({"path": format!("{big_path}/{i}")}),
                )
            })
            .collect();
        let r = vec![ok(json!({}))];
        let corrective = build_corrective(&calls, &r, 3);
        assert!(
            corrective.arguments_summary.len() <= MAX_ARGUMENTS_SUMMARY_CHARS,
            "arguments summary exceeded cap: {} chars",
            corrective.arguments_summary.len(),
        );
    }

    #[test]
    fn the_result_summary_is_capped() {
        let big_body = "y".repeat(1_000);
        let calls = vec![call("read_file", json!({"path": "x"}))];
        let r = vec![ok(json!({"content": big_body}))];
        let corrective = build_corrective(&calls, &r, 3);
        assert!(
            corrective.result_summary.len() <= MAX_RESULT_SUMMARY_CHARS,
            "result summary exceeded cap: {} chars",
            corrective.result_summary.len(),
        );
    }

    #[test]
    fn the_tool_name_is_the_first_in_sorted_order() {
        // A round with multiple tools: the corrective names the
        // first alphabetically. Stable, concrete, no list-building.
        let calls = vec![
            call("write_file", json!({"path": "x"})),
            call("read_file", json!({"path": "y"})),
        ];
        let r = vec![ok(json!({})), ok(json!({}))];
        let corrective = build_corrective(&calls, &r, 3);
        assert_eq!(corrective.tool_name, "read_file");
    }

    #[test]
    fn an_error_result_is_labelled_in_the_summary() {
        let calls = vec![call("read_file", json!({"path": "missing"}))];
        let r = vec![err("no such file")];
        let corrective = build_corrective(&calls, &r, 3);
        assert!(
            corrective.result_summary.starts_with("Error: "),
            "got: {}",
            corrective.result_summary,
        );
    }

    #[test]
    fn a_requires_confirmation_result_is_labelled() {
        let calls = vec![call("write_file", json!({"path": "x"}))];
        let r = vec![ToolResult::RequiresConfirmation {
            description: "confirm write".to_string(),
            callback_id: "cb-1".to_string(),
        }];
        let corrective = build_corrective(&calls, &r, 3);
        assert!(
            corrective.result_summary.starts_with("Confirmation: "),
            "got: {}",
            corrective.result_summary,
        );
    }

    #[test]
    fn multiple_results_are_joined() {
        let calls = vec![
            call("read_file", json!({"path": "a"})),
            call("read_file", json!({"path": "b"})),
        ];
        let r = vec![ok(json!({"content": "A"})), err("b missing")];
        let corrective = build_corrective(&calls, &r, 3);
        assert!(
            corrective.result_summary.contains(" | "),
            "expected a separator between results: {}",
            corrective.result_summary,
        );
    }

    #[test]
    fn the_doc_specific_case_three_greps_of_the_same_pattern() {
        // The scenario §9.4 names: the same grep, three times.
        let mut g = ToolLoopGuard::new();
        let c = vec![call("grep", json!({"path": ".", "pattern": "TODO"}))];
        let r = vec![err("invalid regex: TODO")];
        let _ = g.observe_round(&c, &r);
        let _ = g.observe_round(&c, &r);
        let corrective = g
            .observe_round(&c, &r)
            .expect("third identical grep must fire");
        assert_eq!(corrective.tool_name, "grep");
        assert!(corrective.arguments_summary.contains("grep"));
        assert!(corrective.result_summary.contains("invalid regex"));
    }
}
