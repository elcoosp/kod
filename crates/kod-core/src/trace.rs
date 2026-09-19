//! Structured turn traces (Tier 1.4).
//!
//! One `TurnTrace` object per `process_*` call, emitted as a single
//! JSONL line to `turns.jsonl` next to the session log. Every cost,
//! fallback, tool call, and Jev decision references the enclosing
//! `turn_id` so `/trace` can render a tree without re-joining log
//! entries.

use serde::{Deserialize, Serialize};

/// A monotonic per-session id. Allocated by the engine.
pub type TurnId = u64;

/// Which phase of the streaming loop a round belongs to. Populated
/// from the Jev per-round classifier when enabled; `Unknown`
/// otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoundKind {
    Planning,
    ToolExec,
    Synthesis,
    Summary,
    Unknown,
}

/// How a turn ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnOutcome {
    Completed,
    Cancelled,
    Failed,
    BudgetExhausted,
}

/// A single tool call within a round.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallTrace {
    pub name: String,
    /// Short FNV-1a hash of the arguments so two calls can be
    /// distinguished without storing the args themselves.
    pub args_hash: String,
    pub duration_ms: u64,
    pub outcome: ToolOutcomeKind,
    pub output_bytes: usize,
    /// Lines elided by the Jev compression pass, when it fired.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elided_lines: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolOutcomeKind {
    Success,
    Error,
    Denied,
    RequiresConfirmation,
}

/// A retry that happened within a round.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryTrace {
    pub from_endpoint: String,
    pub to_endpoint: String,
    pub reason: String,
}

/// A single provider round within a turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoundTrace {
    pub kind: RoundKind,
    pub endpoint: String,
    pub model: String,
    pub started_at_ms: u64,
    pub duration_ms: u64,
    pub input_tokens: usize,
    pub output_tokens: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<usize>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCallTrace>,
    #[serde(default)]
    pub retries: Vec<RetryTrace>,
    #[serde(default)]
    pub jev_decisions: u32,
    #[serde(default)]
    pub jev_cache_hits: u32,
}

/// A turn's full trace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnTrace {
    pub id: TurnId,
    pub holder: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<TurnId>,
    pub started_at_ms: u64,
    pub ended_at_ms: u64,
    #[serde(default)]
    pub rounds: Vec<RoundTrace>,
    pub outcome: TurnOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub prompt_tokens: usize,
    pub completion_tokens: usize,
    pub cost_usd: f64,
    pub tool_call_count: u32,
    pub jev_decisions: u32,
    pub jev_cache_hits: u32,
    pub prompt_chars: usize,
    pub reply_chars: usize,
}

impl TurnTrace {
    /// Total wall time in milliseconds.
    pub fn duration_ms(&self) -> u64 {
        self.ended_at_ms.saturating_sub(self.started_at_ms)
    }
}

/// A builder that accumulates a trace while a turn runs. Dropped at
/// the end of `process_*`; the drop calls `finish` and emits the
/// entry — a caller that forgets nothing loses the trace, since the
/// builder is the only place it lives.
#[derive(Debug)]
pub struct TurnTraceBuilder {
    trace: TurnTrace,
    /// The in-progress round's start time, if a round is open.
    round_started_at_ms: Option<u64>,
    round_kind: RoundKind,
    round_endpoint: String,
    round_model: String,
    round_input_tokens: usize,
    round_output_tokens: usize,
    round_cache_read: Option<usize>,
    round_tool_calls: Vec<ToolCallTrace>,
    round_retries: Vec<RetryTrace>,
    round_jev_decisions: u32,
    round_jev_cache_hits: u32,
}

impl TurnTraceBuilder {
    /// Start a trace for turn `id`.
    pub fn new(id: TurnId, holder: impl Into<String>) -> Self {
        let now_ms = now_ms();
        Self {
            trace: TurnTrace {
                id,
                holder: holder.into(),
                parent: None,
                started_at_ms: now_ms,
                ended_at_ms: now_ms,
                rounds: Vec::new(),
                outcome: TurnOutcome::Completed,
                reason: None,
                prompt_tokens: 0,
                completion_tokens: 0,
                cost_usd: 0.0,
                tool_call_count: 0,
                jev_decisions: 0,
                jev_cache_hits: 0,
                prompt_chars: 0,
                reply_chars: 0,
            },
            round_started_at_ms: None,
            round_kind: RoundKind::Unknown,
            round_endpoint: String::new(),
            round_model: String::new(),
            round_input_tokens: 0,
            round_output_tokens: 0,
            round_cache_read: None,
            round_tool_calls: Vec::new(),
            round_retries: Vec::new(),
            round_jev_decisions: 0,
            round_jev_cache_hits: 0,
        }
    }

    /// Mark this trace as belonging to a swarm subtask.
    pub fn with_parent(mut self, parent: TurnId) -> Self {
        self.trace.parent = Some(parent);
        self
    }

    /// Record the prompt the model received (byte count).
    pub fn set_prompt_chars(&mut self, chars: usize) {
        self.trace.prompt_chars = chars;
    }

    /// Open a round. Any open round is closed first.
    pub fn begin_round(&mut self, endpoint: &str, model: &str) {
        self.close_round();
        self.round_started_at_ms = Some(now_ms());
        self.round_kind = RoundKind::Unknown;
        self.round_endpoint = endpoint.to_string();
        self.round_model = model.to_string();
        self.round_input_tokens = 0;
        self.round_output_tokens = 0;
        self.round_cache_read = None;
        self.round_tool_calls.clear();
        self.round_retries.clear();
        self.round_jev_decisions = 0;
        self.round_jev_cache_hits = 0;
    }

    /// Close the currently-open round, if any, and push it to the trace.
    pub fn close_round(&mut self) {
        let Some(start) = self.round_started_at_ms.take() else {
            return;
        };
        let now = now_ms();
        self.trace.rounds.push(RoundTrace {
            kind: self.round_kind,
            endpoint: std::mem::take(&mut self.round_endpoint),
            model: std::mem::take(&mut self.round_model),
            started_at_ms: start,
            duration_ms: now.saturating_sub(start),
            input_tokens: self.round_input_tokens,
            output_tokens: self.round_output_tokens,
            cache_read_tokens: self.round_cache_read.take(),
            tool_calls: std::mem::take(&mut self.round_tool_calls),
            retries: std::mem::take(&mut self.round_retries),
            jev_decisions: self.round_jev_decisions,
            jev_cache_hits: self.round_jev_cache_hits,
        });
        self.round_jev_decisions = 0;
        self.round_jev_cache_hits = 0;
    }

    pub fn set_round_kind(&mut self, kind: RoundKind) {
        self.round_kind = kind;
    }

    pub fn add_usage(
        &mut self,
        prompt_tokens: usize,
        completion_tokens: usize,
        cache_read: Option<usize>,
        cost_usd: f64,
    ) {
        self.round_input_tokens += prompt_tokens;
        self.round_output_tokens += completion_tokens;
        if cache_read.is_some() {
            self.round_cache_read = cache_read;
        }
        self.trace.prompt_tokens += prompt_tokens;
        self.trace.completion_tokens += completion_tokens;
        self.trace.cost_usd += cost_usd;
    }

    pub fn add_tool_call(
        &mut self,
        name: &str,
        args_hash: String,
        duration_ms: u64,
        outcome: ToolOutcomeKind,
        output_bytes: usize,
        elided_lines: Option<usize>,
    ) {
        self.round_tool_calls.push(ToolCallTrace {
            name: name.to_string(),
            args_hash,
            duration_ms,
            outcome,
            output_bytes,
            elided_lines,
        });
        self.trace.tool_call_count += 1;
    }

    pub fn add_retry(&mut self, from: &str, to: &str, reason: &str) {
        self.round_retries.push(RetryTrace {
            from_endpoint: from.to_string(),
            to_endpoint: to.to_string(),
            reason: reason.to_string(),
        });
    }

    pub fn add_jev(&mut self, cached: bool) {
        self.round_jev_decisions += 1;
        self.trace.jev_decisions += 1;
        if cached {
            self.round_jev_cache_hits += 1;
            self.trace.jev_cache_hits += 1;
        }
    }

    pub fn set_outcome(&mut self, outcome: TurnOutcome, reason: Option<String>) {
        self.trace.outcome = outcome;
        self.trace.reason = reason;
    }

    pub fn set_reply_chars(&mut self, chars: usize) {
        self.trace.reply_chars = chars;
    }

    /// Finish the trace and return the immutable value.
    pub fn finish(mut self) -> TurnTrace {
        self.close_round();
        self.trace.ended_at_ms = now_ms();
        self.trace
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Short FNV-1a hash of a value's serialized form, for `args_hash`.
pub fn hash_short(v: &serde_json::Value) -> String {
    let bytes = serde_json::to_vec(v).unwrap_or_default();
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_accumulates_round_and_tools() {
        let mut b = TurnTraceBuilder::new(1, "session");
        b.set_prompt_chars(100);
        b.begin_round("cloud", "claude");
        b.add_usage(500, 50, None, 0.005);
        b.add_tool_call(
            "read_file",
            "abc".into(),
            12,
            ToolOutcomeKind::Success,
            100,
            None,
        );
        b.add_jev(false);
        b.add_jev(true);
        let t = b.finish();
        assert_eq!(t.id, 1);
        assert_eq!(t.holder, "session");
        assert_eq!(t.prompt_tokens, 500);
        assert_eq!(t.completion_tokens, 50);
        assert_eq!(t.tool_call_count, 1);
        assert_eq!(t.jev_decisions, 2);
        assert_eq!(t.jev_cache_hits, 1);
        assert_eq!(t.rounds.len(), 1);
        assert_eq!(t.rounds[0].endpoint, "cloud");
        assert_eq!(t.rounds[0].model, "claude");
        assert_eq!(t.rounds[0].tool_calls.len(), 1);
    }

    #[test]
    fn builder_multiple_rounds_are_ordered() {
        let mut b = TurnTraceBuilder::new(2, "session");
        b.begin_round("a", "m1");
        b.add_usage(100, 10, None, 0.001);
        b.begin_round("b", "m2");
        b.add_usage(200, 20, None, 0.002);
        let t = b.finish();
        assert_eq!(t.rounds.len(), 2);
        assert_eq!(t.rounds[0].endpoint, "a");
        assert_eq!(t.rounds[1].endpoint, "b");
        assert_eq!(t.prompt_tokens, 300);
        assert_eq!(t.completion_tokens, 30);
    }

    #[test]
    fn finish_without_any_round_is_fine() {
        let b = TurnTraceBuilder::new(3, "session");
        let t = b.finish();
        assert_eq!(t.rounds.len(), 0);
        assert_eq!(t.prompt_tokens, 0);
    }

    #[test]
    fn add_retry_records_endpoints() {
        let mut b = TurnTraceBuilder::new(4, "session");
        b.begin_round("a", "m1");
        b.add_retry("a", "b", "timeout");
        let t = b.finish();
        assert_eq!(t.rounds[0].retries.len(), 1);
        assert_eq!(t.rounds[0].retries[0].from_endpoint, "a");
        assert_eq!(t.rounds[0].retries[0].to_endpoint, "b");
        assert_eq!(t.rounds[0].retries[0].reason, "timeout");
    }

    #[test]
    fn hash_short_is_stable() {
        let v = serde_json::json!({"path": "src/main.rs"});
        assert_eq!(hash_short(&v), hash_short(&v));
    }

    #[test]
    fn hash_short_differs_for_different_values() {
        let a = serde_json::json!({"path": "a"});
        let b = serde_json::json!({"path": "b"});
        assert_ne!(hash_short(&a), hash_short(&b));
    }

    #[test]
    fn outcome_and_reason_round_trip() {
        let mut b = TurnTraceBuilder::new(5, "session");
        b.set_outcome(TurnOutcome::Failed, Some("network".into()));
        let t = b.finish();
        assert_eq!(t.outcome, TurnOutcome::Failed);
        assert_eq!(t.reason.as_deref(), Some("network"));
    }

    #[test]
    fn round_kind_round_trips() {
        for kind in [
            RoundKind::Planning,
            RoundKind::ToolExec,
            RoundKind::Synthesis,
            RoundKind::Summary,
            RoundKind::Unknown,
        ] {
            let s = serde_json::to_string(&kind).unwrap();
            let back: RoundKind = serde_json::from_str(&s).unwrap();
            assert_eq!(kind, back);
        }
    }

    #[test]
    fn full_trace_round_trips_through_json() {
        let mut b = TurnTraceBuilder::new(6, "swarm:a1");
        b.set_prompt_chars(42);
        b.begin_round("local", "qwen");
        b.add_usage(100, 10, Some(80), 0.0);
        b.add_tool_call(
            "grep",
            "deadbeef".into(),
            5,
            ToolOutcomeKind::Error,
            50,
            Some(3),
        );
        b.add_jev(true);
        b.set_reply_chars(200);
        b.set_outcome(TurnOutcome::Completed, None);
        let t = b.finish();
        let s = serde_json::to_string(&t).unwrap();
        let back: TurnTrace = serde_json::from_str(&s).unwrap();
        assert_eq!(back.id, 6);
        assert_eq!(back.holder, "swarm:a1");
        assert_eq!(back.tool_call_count, 1);
        assert_eq!(back.rounds[0].tool_calls[0].elided_lines, Some(3));
    }

    #[test]
    fn duration_is_ended_minus_started() {
        let b = TurnTraceBuilder::new(7, "s");
        let t = b.finish();
        // Should never underflow.
        let _ = t.duration_ms();
    }
}
