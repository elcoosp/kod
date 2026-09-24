//! Overnight runs: a manifest, a phase machine, and a task-card report.
//!
//! An overnight run is a swarm run with three differences: it has a
//! deadline it must respect, it must stop *starting* work early enough
//! to report what it did, and its output is a document a person reads
//! over coffee rather than a chat reply.
//!
//! This module is the policy layer. It does not spawn agents — the
//! swarm runner does. It answers three questions the runner asks:
//!
//! 1. **Which phase are we in?** ([`Phase`], advanced by wall clock.)
//! 2. **May a new subtask start?** (No, once the phase is past
//!    `Running`.)
//! 3. **What happened?** ([`TaskCard`], one per completed subtask.)
//!
//! The task card is a *join*, not new data: the subtask's own fields,
//! the agent's completion report, the files it wrote, and the highest
//! risk any of its commands carried. Every one of those already exists
//! by the time the runner finishes a wave.

use serde::{Deserialize, Serialize};

use crate::swarm_runner::{AgentResult, AgentOutcome, Subtask};

/// Where an overnight run is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Normal operation: subtasks may be dispatched.
    Running,
    /// Past the handoff point: no new subtasks, in-flight agents are
    /// told to wrap up. The runner reaches this with enough time left
    /// to collect results and write the report.
    WindDown,
    /// Agents are done; the report is being assembled.
    MorningReport,
    /// The report is written; a waiting process may exit.
    Done,
}

impl Phase {
    /// Whether a new subtask may be dispatched in this phase.
    ///
    /// Only `Running` allows it. A prompt asking the model to wind
    /// down is advice; refusing to dispatch is enforcement, and only
    /// the second one actually bounds the run.
    pub fn allows_new_work(self) -> bool {
        matches!(self, Phase::Running)
    }
}

/// The parameters of an overnight run.
#[derive(Debug, Clone)]
pub struct OvernightManifest {
    pub mission: String,
    /// When the user wants to see the result. The run must be done
    /// (report written, artifacts on disk) by then.
    pub target_wake_at: time::OffsetDateTime,
    /// When to stop starting work. `target - min(30m, duration/4)`,
    /// so a short run still reserves a quarter of its time for the
    /// report and a long one reserves at most half an hour.
    pub handoff_ready_at: time::OffsetDateTime,
    /// Where the report and per-task artifacts go.
    pub artifacts_dir: std::path::PathBuf,
}

impl OvernightManifest {
    /// Build a manifest for a run of `duration` starting now.
    ///
    /// The handoff margin is `min(30m, duration/4)`: an 8-hour run
    /// stops starting work 30 minutes before the target; a 40-minute
    /// run stops 10 minutes before it. The rule is "never less than a
    /// quarter of the run for the report" for short runs, and "never
    /// more than half an hour" for long ones.
    pub fn for_duration(
        mission: impl Into<String>,
        duration: time::Duration,
        artifacts_dir: impl Into<std::path::PathBuf>,
        now: time::OffsetDateTime,
    ) -> Self {
        let margin_secs = (duration.whole_seconds() / 4).min(30 * 60);
        let margin = time::Duration::seconds(margin_secs);
        let target_wake_at = now + duration;
        Self {
            mission: mission.into(),
            target_wake_at,
            handoff_ready_at: target_wake_at - margin,
            artifacts_dir: artifacts_dir.into(),
        }
    }

    /// Build a manifest for a run of `duration` starting now.
    ///
    /// A convenience over [`Self::for_duration`] so a caller that does
    /// not depend on `time` — the TUI — can construct one without
    /// taking the dependency for a single `now_utc()` call.
    pub fn starting_now(
        mission: impl Into<String>,
        duration: std::time::Duration,
        artifacts_dir: impl Into<std::path::PathBuf>,
    ) -> Self {
        // The std type is what a caller without the `time` dependency
        // produces. Seconds is the common unit; a sub-second overnight
        // duration is not a thing.
        let as_time = time::Duration::seconds(duration.as_secs() as i64);
        Self::for_duration(
            mission,
            as_time,
            artifacts_dir,
            time::OffsetDateTime::now_utc(),
        )
    }

    /// The phase `now` falls in, given whether the report is done.
    ///
    /// The clock alone decides `Running` vs `WindDown`; a caller that
    /// has finished the report passes `report_done` to move past it.
    pub fn phase_at(&self, now: time::OffsetDateTime, report_done: bool) -> Phase {
        if report_done {
            return Phase::Done;
        }
        if now >= self.target_wake_at {
            return Phase::MorningReport;
        }
        if now >= self.handoff_ready_at {
            return Phase::WindDown;
        }
        Phase::Running
    }
}

/// One subtask's outcome, as a card a person reads.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskCard {
    pub name: String,
    pub description: String,
    /// What the agent says it did.
    pub summary: String,
    /// Commands it ran and what came back, when it reported any.
    pub validation: Option<String>,
    /// Open questions or next steps it raised.
    pub followups: Vec<String>,
    /// Files it wrote, relative to the repo root.
    pub files_written: Vec<String>,
    /// The highest risk any of its commands carried, when the run
    /// classified any. `None` when the agent ran no commands, or the
    /// risk gate is disabled.
    pub max_risk: Option<String>,
    /// Whether the subtask finished or failed.
    pub status: CardStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CardStatus {
    Done,
    Failed,
}

/// Build a task card from what the runner already has.
///
/// Pure: every input is a value the runner holds by the time a wave
/// completes, so this is a join, not a new data path.
pub fn build_task_card(
    subtask: &Subtask,
    result: &AgentResult,
    files_written: &[String],
    max_risk: Option<&str>,
) -> TaskCard {
    // The agent's final text is where the `<completion-report>` block
    // lives — the same contract `swarm_runner` reads for followups.
    // `Completed` carries that text; parsing it here is the whole
    // reason the card is a *join* rather than a copy.
    let (summary, validation, followups, status) = match &result.outcome {
        AgentOutcome::Completed(text) => {
            let report = kod_swarm::completion_report::parse(text);
            (
                report.summary,
                report.validation,
                report.followups,
                CardStatus::Done,
            )
        }
        AgentOutcome::Failed(err) => (
            err.clone(),
            None,
            Vec::new(),
            CardStatus::Failed,
        ),
    };

    TaskCard {
        name: subtask.name.clone(),
        description: subtask.description.clone(),
        summary,
        validation,
        followups,
        files_written: files_written.to_vec(),
        max_risk: max_risk.map(str::to_string),
        status,
    }
}

/// Render the cards as the markdown report a person reads in the
/// morning.
///
/// Markdown rather than HTML: the TUI renders it, the session log can
/// hold it, and a template engine would be one more dependency for a
/// document whose shape is fixed.
pub fn render_report(mission: &str, cards: &[TaskCard], merged: &str) -> String {
    let done = cards.iter().filter(|c| c.status == CardStatus::Done).count();
    let failed = cards.len() - done;

    let mut out = format!("# Overnight: {mission}\n\n");
    out.push_str(&format!(
        "{done} of {} subtasks completed",
        cards.len(),
    ));
    if failed > 0 {
        out.push_str(&format!(", {failed} failed"));
    }
    out.push_str(".\n\n");

    // The merged synthesis leads: it is the answer to the mission,
    // and the cards are the evidence.
    if !merged.trim().is_empty() {
        out.push_str("## Result\n\n");
        out.push_str(merged.trim());
        out.push_str("\n\n");
    }

    out.push_str("## Subtasks\n\n");
    for card in cards {
        let mark = match card.status {
            CardStatus::Done => "[x]",
            CardStatus::Failed => "[ ]",
        };
        out.push_str(&format!("{mark} **{}**\n\n", card.name));
        out.push_str(&format!("{}\n\n", card.summary.trim()));
        if let Some(v) = &card.validation {
            out.push_str(&format!("*Validation:* {}\n\n", v.trim()));
        }
        if !card.files_written.is_empty() {
            out.push_str("*Files:* ");
            out.push_str(&card.files_written.join(", "));
            out.push_str("\n\n");
        }
        if let Some(r) = &card.max_risk
            && r != "safe"
            && r != "low"
        {
            out.push_str(&format!("*Risk:* {r}\n\n"));
        }
        if !card.followups.is_empty() {
            out.push_str("*Open questions:*\n");
            for f in &card.followups {
                out.push_str(&format!("- {}\n", f.trim()));
            }
            out.push('\n');
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::swarm_runner::{AgentOutcome, Subtask};
    use kod_swarm::Capability;
    use kod_types::AgentId;

    fn subtask(name: &str) -> Subtask {
        Subtask {
            name: name.to_string(),
            description: format!("do {name}"),
            expected_writes: vec![],
            capability: Capability::Coding,
            depends_on: vec![],
        }
    }

    fn success(id: &str, summary: &str) -> AgentResult {
        // The agent's final text carries the completion-report block.
        let text = format!(
            "Here is what I did.\n\n<completion-report status=done>\n\
             summary: {summary}\nvalidation: cargo check (clean)\n\
             followups: wire the metric\n</completion-report>\n",
        );
        AgentResult {
            id: AgentId::new(),
            name: id.to_string(),
            subtask: "x".to_string(),
            outcome: AgentOutcome::Completed(text),
        }
    }

    fn failed(id: &str, err: &str) -> AgentResult {
        AgentResult {
            id: AgentId::new(),
            name: id.to_string(),
            subtask: "x".to_string(),
            outcome: AgentOutcome::Failed(err.to_string()),
        }
    }

    #[test]
    fn only_running_allows_new_work() {
        assert!(Phase::Running.allows_new_work());
        for p in [Phase::WindDown, Phase::MorningReport, Phase::Done] {
            assert!(!p.allows_new_work(), "{p:?} must not start work");
        }
    }

    #[test]
    fn the_manifest_margin_is_capped_at_half_an_hour() {
        let now = time::OffsetDateTime::UNIX_EPOCH;
        let m = OvernightManifest::for_duration(
            "x",
            time::Duration::hours(8),
            "/tmp",
            now,
        );
        // 8 hours / 4 = 2 hours, capped at 30 minutes.
        let margin = m.target_wake_at - m.handoff_ready_at;
        assert_eq!(margin, time::Duration::minutes(30));
    }

    #[test]
    fn the_manifest_margin_is_a_quarter_for_short_runs() {
        let now = time::OffsetDateTime::UNIX_EPOCH;
        let m = OvernightManifest::for_duration(
            "x",
            time::Duration::minutes(40),
            "/tmp",
            now,
        );
        // 40 minutes / 4 = 10 minutes, under the cap.
        let margin = m.target_wake_at - m.handoff_ready_at;
        assert_eq!(margin, time::Duration::minutes(10));
    }

    #[test]
    fn the_phase_moves_with_the_clock() {
        let now = time::OffsetDateTime::UNIX_EPOCH;
        let m = OvernightManifest::for_duration(
            "x",
            time::Duration::hours(2),
            "/tmp",
            now,
        );
        // Before the handoff point.
        assert_eq!(m.phase_at(now, false), Phase::Running);
        // At the handoff point.
        assert_eq!(m.phase_at(m.handoff_ready_at, false), Phase::WindDown);
        // At the target.
        assert_eq!(m.phase_at(m.target_wake_at, false), Phase::MorningReport);
        // With the report written.
        assert_eq!(m.phase_at(now, true), Phase::Done);
    }

    #[test]
    fn a_card_carries_the_report_fields() {
        let st = subtask("parse");
        let r = success("agent-1", "wrote the parser");
        let card = build_task_card(
            &st,
            &r,
            &["src/parse.rs".to_string()],
            Some("confirm"),
        );
        assert_eq!(card.name, "parse");
        assert_eq!(card.summary, "wrote the parser");
        assert_eq!(card.validation.as_deref(), Some("cargo check (clean)"));
        assert_eq!(card.followups, vec!["wire the metric".to_string()]);
        assert_eq!(card.files_written, vec!["src/parse.rs".to_string()]);
        assert_eq!(card.max_risk.as_deref(), Some("confirm"));
        assert_eq!(card.status, CardStatus::Done);
    }

    #[test]
    fn a_failed_card_carries_the_error_as_its_summary() {
        let st = subtask("parse");
        let r = failed("agent-1", "timed out after 300s");
        let card = build_task_card(&st, &r, &[], None);
        assert_eq!(card.status, CardStatus::Failed);
        assert_eq!(card.summary, "timed out after 300s");
        assert!(card.validation.is_none());
        assert!(card.followups.is_empty());
    }

    #[test]
    fn the_report_counts_done_and_failed() {
        let cards = vec![
            build_task_card(&subtask("a"), &success("a", "did a"), &[], None),
            build_task_card(&subtask("b"), &failed("b", "boom"), &[], None),
        ];
        let out = render_report("ship it", &cards, "merged answer");
        assert!(out.contains("1 of 2 subtasks completed"));
        assert!(out.contains("1 failed"));
    }

    #[test]
    fn the_report_leads_with_the_merged_result() {
        let cards = vec![build_task_card(
            &subtask("a"),
            &success("a", "did a"),
            &[],
            None,
        )];
        let out = render_report("m", &cards, "the answer is 42");
        let result_at = out.find("the answer is 42").expect("merged is present");
        let subtasks_at = out.find("## Subtasks").expect("section present");
        assert!(result_at < subtasks_at, "merged leads the cards");
    }

    #[test]
    fn the_report_marks_done_and_failed_cards_differently() {
        let cards = vec![
            build_task_card(&subtask("a"), &success("a", "did a"), &[], None),
            build_task_card(&subtask("b"), &failed("b", "boom"), &[], None),
        ];
        let out = render_report("m", &cards, "");
        assert!(out.contains("[x] **a**"));
        assert!(out.contains("[ ] **b**"));
    }

    #[test]
    fn a_low_risk_card_omits_the_risk_line() {
        // The report is for a person scanning it; a line saying
        // "risk: safe" on every card is noise.
        let cards = vec![build_task_card(
            &subtask("a"),
            &success("a", "did a"),
            &[],
            Some("safe"),
        )];
        let out = render_report("m", &cards, "");
        assert!(!out.contains("*Risk:*"));
    }

    #[test]
    fn a_high_risk_card_shows_the_risk() {
        let cards = vec![build_task_card(
            &subtask("a"),
            &success("a", "did a"),
            &[],
            Some("catastrophic"),
        )];
        let out = render_report("m", &cards, "");
        assert!(out.contains("*Risk:* catastrophic"));
    }
}
