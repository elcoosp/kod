//! Run collector: per-run metadata (borrow from oh-my-pi, delta §9.11).
//!
//! # The gap
//!
//! The engine records timings and outcomes in many places — the
//! `TurnTraceBuilder` for a turn, `elapsed_ms` for a tool call, the
//! `TaskResponse` for a request. Nothing answers "how did this run
//! go?" in one place. A developer reading `/stats` sees a
//! summary; a developer diagnosing a run has to walk several
//! structures.
//!
//! # What this collects
//!
//! A [`RunCollector`] is updated once per turn and once per tool
//! call. At any point it answers the design's questions:
//!
//! * per-`stopReason` counts (`end_turn`, `max_tokens`, `tool_use`, …)
//! * per-tool status: ok / error / skipped / blocked / timeout / aborted
//! * per-tool invocation counters
//! * coverage: tools available, tools invoked, tools unused
//! * cost-unavailable reasons: *why* a cost could not be computed
//!
//! # What this is NOT
//!
//! * Not a trace. The `TurnTraceBuilder` is the detailed record;
//!   this is the aggregate.
//! * Not persisted. The collector lives for the run; a caller that
//!   wants it durable writes the `report()` output.
//! * Not a policy input. Nothing in the engine branches on these
//!   numbers today. They are for a human reading `/stats`.

use std::collections::{BTreeMap, BTreeSet};

/// How a tool call ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ToolStatus {
    /// The call succeeded and produced a result.
    Ok,
    /// The call returned an error result — the model's mistake, not
    /// the tool's.
    Error,
    /// The call was denied by a pre-tool hook.
    Blocked,
    /// The call hit a quota cap.
    Skipped,
    /// The call timed out.
    Timeout,
    /// The call was aborted mid-flight (cancel, pause-and-stop).
    Aborted,
}

impl ToolStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Error => "error",
            Self::Blocked => "blocked",
            Self::Skipped => "skipped",
            Self::Timeout => "timeout",
            Self::Aborted => "aborted",
        }
    }
}

/// Why a cost could not be computed.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum CostUnavailable {
    /// The provider did not report usage.
    NoUsage,
    /// The endpoint has no pricing configured.
    NoPricing,
    /// Some usage was reported but the numbers were nonsensical.
    InvalidUsage,
}

impl CostUnavailable {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NoUsage => "no-usage",
            Self::NoPricing => "no-pricing",
            Self::InvalidUsage => "invalid-usage",
        }
    }
}

/// Per-tool counters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolCounts {
    pub ok: u64,
    pub error: u64,
    pub blocked: u64,
    pub skipped: u64,
    pub timeout: u64,
    pub aborted: u64,
}

impl ToolCounts {
    pub fn record(&mut self, status: ToolStatus) {
        match status {
            ToolStatus::Ok => self.ok += 1,
            ToolStatus::Error => self.error += 1,
            ToolStatus::Blocked => self.blocked += 1,
            ToolStatus::Skipped => self.skipped += 1,
            ToolStatus::Timeout => self.timeout += 1,
            ToolStatus::Aborted => self.aborted += 1,
        }
    }

    pub fn total(&self) -> u64 {
        self.ok + self.error + self.blocked + self.skipped + self.timeout + self.aborted
    }
}

/// The aggregate for one run.
#[derive(Debug, Default)]
pub struct RunCollector {
    /// stopReason → count. The reason string is the provider's
    /// spelling (`"end_turn"`, `"max_tokens"`, `"tool_use"`, …).
    stop_reasons: BTreeMap<String, u64>,
    /// tool name → per-status counters.
    tool_counts: BTreeMap<String, ToolCounts>,
    /// Every tool the run had available (from the tools array).
    tools_available: BTreeSet<String>,
    /// Every tool actually invoked.
    tools_invoked: BTreeSet<String>,
    /// How many cost-unavailable reasons were recorded, by reason.
    cost_unavailable: BTreeMap<CostUnavailable, u64>,
    /// Total turns observed.
    turns: u64,
    /// Total wall time of the run in milliseconds. Summed from
    /// per-turn observations; the caller supplies each turn's
    /// elapsed.
    elapsed_ms: u64,
}

impl RunCollector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Note the tools the run has available. Called once at the
    /// start of a run, or refreshed when the surface changes.
    pub fn note_available_tools<I, S>(&mut self, names: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        for n in names {
            self.tools_available.insert(n.into());
        }
    }

    /// Record a turn: its stop reason, wall time, and whether usage
    /// was available.
    pub fn observe_turn(
        &mut self,
        stop_reason: Option<&str>,
        elapsed_ms: u64,
        cost_unavailable: Option<CostUnavailable>,
    ) {
        self.turns += 1;
        self.elapsed_ms = self.elapsed_ms.saturating_add(elapsed_ms);
        if let Some(r) = stop_reason {
            *self.stop_reasons.entry(r.to_string()).or_insert(0) += 1;
        }
        if let Some(reason) = cost_unavailable {
            *self.cost_unavailable.entry(reason).or_insert(0) += 1;
        }
    }

    /// Record one tool call.
    pub fn observe_tool(&mut self, name: &str, status: ToolStatus) {
        self.tools_invoked.insert(name.to_string());
        self.tool_counts.entry(name.to_string()).or_default().record(status);
    }

    /// How many turns the run took.
    pub fn turns(&self) -> u64 {
        self.turns
    }

    /// Total elapsed wall time, milliseconds.
    pub fn elapsed_ms(&self) -> u64 {
        self.elapsed_ms
    }

    /// A copy of the stop-reason histogram.
    pub fn stop_reasons(&self) -> BTreeMap<String, u64> {
        self.stop_reasons.clone()
    }

    /// A copy of the per-tool counters.
    pub fn tool_counts(&self) -> BTreeMap<String, ToolCounts> {
        self.tool_counts.clone()
    }

    /// Tools available but never invoked. Sorted.
    pub fn unused_tools(&self) -> Vec<String> {
        self.tools_available
            .difference(&self.tools_invoked)
            .cloned()
            .collect()
    }

    /// A one-line coverage summary: `invoked / available`.
    pub fn coverage(&self) -> (usize, usize) {
        (self.tools_invoked.len(), self.tools_available.len())
    }

    /// The full report, as a multi-line string a `/stats` handler
    /// can print.
    pub fn report(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "run: {} turn(s), {} ms total\n",
            self.turns, self.elapsed_ms,
        ));
        if !self.stop_reasons.is_empty() {
            out.push_str("stop reasons:\n");
            for (r, n) in &self.stop_reasons {
                out.push_str(&format!("  {r}: {n}\n"));
            }
        }
        if !self.tool_counts.is_empty() {
            out.push_str("tools:\n");
            for (name, c) in &self.tool_counts {
                out.push_str(&format!(
                    "  {name}: {} ok, {} err, {} blocked, {} skipped, {} timeout, {} aborted\n",
                    c.ok, c.error, c.blocked, c.skipped, c.timeout, c.aborted,
                ));
            }
        }
        let (invoked, available) = self.coverage();
        if available > 0 {
            out.push_str(&format!("coverage: {invoked} / {available} tools used\n"));
            let unused = self.unused_tools();
            if !unused.is_empty() {
                out.push_str(&format!("  unused: {}\n", unused.join(", ")));
            }
        }
        if !self.cost_unavailable.is_empty() {
            out.push_str("cost unavailable:\n");
            for (r, n) in &self.cost_unavailable {
                out.push_str(&format!("  {}: {n}\n", r.as_str()));
            }
        }
        out.trim_end().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_collector_reports_zero_turns() {
        let c = RunCollector::new();
        assert_eq!(c.turns(), 0);
        assert_eq!(c.elapsed_ms(), 0);
        assert!(c.stop_reasons().is_empty());
        assert!(c.tool_counts().is_empty());
    }

    #[test]
    fn observing_turns_accumulates_time() {
        let mut c = RunCollector::new();
        c.observe_turn(Some("end_turn"), 100, None);
        c.observe_turn(Some("end_turn"), 200, None);
        assert_eq!(c.turns(), 2);
        assert_eq!(c.elapsed_ms(), 300);
    }

    #[test]
    fn stop_reasons_are_histogrammed() {
        let mut c = RunCollector::new();
        c.observe_turn(Some("end_turn"), 0, None);
        c.observe_turn(Some("tool_use"), 0, None);
        c.observe_turn(Some("end_turn"), 0, None);
        let r = c.stop_reasons();
        assert_eq!(r.get("end_turn"), Some(&2));
        assert_eq!(r.get("tool_use"), Some(&1));
    }

    #[test]
    fn a_turn_with_no_stop_reason_is_still_counted() {
        let mut c = RunCollector::new();
        c.observe_turn(None, 50, None);
        assert_eq!(c.turns(), 1);
        assert!(c.stop_reasons().is_empty());
    }

    #[test]
    fn tool_statuses_are_counted_per_tool() {
        let mut c = RunCollector::new();
        c.observe_tool("read_file", ToolStatus::Ok);
        c.observe_tool("read_file", ToolStatus::Ok);
        c.observe_tool("read_file", ToolStatus::Error);
        c.observe_tool("grep", ToolStatus::Ok);
        let counts = c.tool_counts();
        let rf = &counts["read_file"];
        assert_eq!(rf.ok, 2);
        assert_eq!(rf.error, 1);
        assert_eq!(rf.total(), 3);
        assert_eq!(counts["grep"].ok, 1);
    }

    #[test]
    fn every_status_has_a_counter() {
        let mut c = RunCollector::new();
        for s in [
            ToolStatus::Ok,
            ToolStatus::Error,
            ToolStatus::Blocked,
            ToolStatus::Skipped,
            ToolStatus::Timeout,
            ToolStatus::Aborted,
        ] {
            c.observe_tool("t", s);
        }
        let counts = &c.tool_counts()["t"];
        assert_eq!(counts.ok, 1);
        assert_eq!(counts.error, 1);
        assert_eq!(counts.blocked, 1);
        assert_eq!(counts.skipped, 1);
        assert_eq!(counts.timeout, 1);
        assert_eq!(counts.aborted, 1);
        assert_eq!(counts.total(), 6);
    }

    #[test]
    fn coverage_tracks_available_vs_invoked() {
        let mut c = RunCollector::new();
        c.note_available_tools(["read_file", "write_file", "grep"]);
        c.observe_tool("read_file", ToolStatus::Ok);
        c.observe_tool("grep", ToolStatus::Ok);
        assert_eq!(c.coverage(), (2, 3));
        assert_eq!(c.unused_tools(), vec!["write_file".to_string()]);
    }

    #[test]
    fn a_tool_invoked_but_not_declared_still_counts_as_invoked() {
        let mut c = RunCollector::new();
        c.note_available_tools(["read_file"]);
        c.observe_tool("grep", ToolStatus::Ok);
        // grep is invoked but not declared; read_file is declared but
        // never invoked. Coverage is 1 invoked / 1 available, and
        // read_file is the unused one.
        assert_eq!(c.coverage(), (1, 1));
        assert_eq!(c.unused_tools(), vec!["read_file".to_string()]);
    }

    #[test]
    fn cost_unavailable_reasons_are_histogrammed() {
        let mut c = RunCollector::new();
        c.observe_turn(Some("end_turn"), 0, Some(CostUnavailable::NoPricing));
        c.observe_turn(Some("end_turn"), 0, Some(CostUnavailable::NoUsage));
        c.observe_turn(Some("end_turn"), 0, Some(CostUnavailable::NoPricing));
        // A turn with cost is not counted.
        c.observe_turn(Some("end_turn"), 0, None);
        let r = c.report();
        assert!(r.contains("no-pricing: 2"), "got: {r}");
        assert!(r.contains("no-usage: 1"), "got: {r}");
    }

    #[test]
    fn the_report_names_turns_and_time() {
        let mut c = RunCollector::new();
        c.observe_turn(Some("end_turn"), 1234, None);
        let r = c.report();
        assert!(r.contains("1 turn(s)"), "got: {r}");
        assert!(r.contains("1234 ms"), "got: {r}");
    }

    #[test]
    fn the_report_lists_tools_and_statuses() {
        let mut c = RunCollector::new();
        c.observe_tool("read_file", ToolStatus::Ok);
        c.observe_tool("read_file", ToolStatus::Error);
        let r = c.report();
        assert!(r.contains("read_file"), "got: {r}");
        assert!(r.contains("1 ok"), "got: {r}");
        assert!(r.contains("1 err"), "got: {r}");
    }

    #[test]
    fn the_report_lists_unused_tools() {
        let mut c = RunCollector::new();
        c.note_available_tools(["read_file", "never_used"]);
        c.observe_tool("read_file", ToolStatus::Ok);
        let r = c.report();
        assert!(r.contains("unused: never_used"), "got: {r}");
    }

    #[test]
    fn a_run_with_no_coverage_info_does_not_print_the_section() {
        let mut c = RunCollector::new();
        c.observe_turn(Some("end_turn"), 10, None);
        let r = c.report();
        assert!(!r.contains("coverage:"), "got: {r}");
    }
}
