//! Durable plan artifact (Tier 2.1).
//!
//! A `Plan` is a step-by-step shape for a complex task. It is
//! created once at the start of the first turn and re-rendered at
//! the top of every subsequent system prompt, so the model has a
//! stable target instead of re-deriving one from the transcript.
//!
//! Two update paths:
//!
//! * Explicit — the model calls a `plan_update` tool whose arguments
//!   match `PlanUpdate`.
//! * Implicit — every N rounds, a Jev batch classifies which steps
//!   are complete. A drift indicator appears in the plan panel when
//!   the model's output stopped matching its own step.
//!
//! The plan is per-transcript (keyed the same as history) so a
//! swarm agent carries its own.

use serde::{Deserialize, Serialize};

/// One step in a plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanStep {
    pub id: u32,
    pub text: String,
    pub status: PlanStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rationale: Option<String>,
    #[serde(default)]
    pub depends_on: Vec<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    #[default]
    Pending,
    InProgress,
    Done,
    Blocked,
    Skipped,
}

/// A plan for one transcript.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub goal: String,
    pub steps: Vec<PlanStep>,
    pub current: usize,
    #[serde(default)]
    pub notes: std::collections::BTreeMap<u32, Vec<String>>,
    #[serde(default)]
    pub created_at_ms: u64,
    #[serde(default)]
    pub jev_confidence: f32,
}

impl Plan {
    /// Create a fresh plan from a goal and a list of step texts.
    pub fn new(goal: impl Into<String>, step_texts: Vec<String>) -> Self {
        let steps = step_texts
            .into_iter()
            .enumerate()
            .map(|(i, text)| PlanStep {
                id: i as u32,
                text,
                status: if i == 0 {
                    PlanStatus::InProgress
                } else {
                    PlanStatus::Pending
                },
                rationale: None,
                depends_on: Vec::new(),
            })
            .collect();
        Self {
            goal: goal.into(),
            steps,
            current: 0,
            notes: Default::default(),
            created_at_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
            jev_confidence: 0.0,
        }
    }

    /// The current step, if any.
    pub fn current_step(&self) -> Option<&PlanStep> {
        self.steps.get(self.current)
    }

    /// Advance: mark the current step done and move to the next
    /// pending step. No-op when the plan has no more steps.
    pub fn advance(&mut self) {
        if let Some(s) = self.steps.get_mut(self.current) {
            s.status = PlanStatus::Done;
        }
        // Find the next pending step (may be gated by depends_on).
        for (i, s) in self.steps.iter_mut().enumerate() {
            if s.status == PlanStatus::Pending {
                s.status = PlanStatus::InProgress;
                self.current = i;
                return;
            }
        }
        // No pending steps left — mark the plan complete by parking
        // on the last index.
        if !self.steps.is_empty() {
            self.current = self.steps.len() - 1;
        }
    }

    /// Mark a specific step by id.
    pub fn set_status(&mut self, id: u32, status: PlanStatus) {
        if let Some(s) = self.steps.iter_mut().find(|s| s.id == id) {
            s.status = status;
        }
    }

    /// Insert a new step after `after_id`.
    pub fn insert_after(&mut self, after_id: u32, text: String) -> u32 {
        let new_id = self.steps.iter().map(|s| s.id).max().unwrap_or(0) + 1;
        let pos = self
            .steps
            .iter()
            .position(|s| s.id == after_id)
            .map(|i| i + 1)
            .unwrap_or(self.steps.len());
        self.steps.insert(
            pos,
            PlanStep {
                id: new_id,
                text,
                status: PlanStatus::Pending,
                rationale: None,
                depends_on: Vec::new(),
            },
        );
        new_id
    }

    /// Remove a step by id.
    pub fn remove(&mut self, id: u32) {
        self.steps.retain(|s| s.id != id);
        if self.current >= self.steps.len() && !self.steps.is_empty() {
            self.current = self.steps.len() - 1;
        }
    }

    /// Replace a step's text.
    pub fn replace_text(&mut self, id: u32, text: String) {
        if let Some(s) = self.steps.iter_mut().find(|s| s.id == id) {
            s.text = text;
        }
    }

    /// Add a note to a step.
    pub fn annotate(&mut self, id: u32, note: String) {
        self.notes.entry(id).or_default().push(note);
    }

    /// Fraction of steps that are Done or Skipped.
    pub fn progress(&self) -> f32 {
        if self.steps.is_empty() {
            return 1.0;
        }
        let done = self
            .steps
            .iter()
            .filter(|s| matches!(s.status, PlanStatus::Done | PlanStatus::Skipped))
            .count();
        done as f32 / self.steps.len() as f32
    }

    /// Render the plan as it appears at the top of the system prompt.
    /// Kept compact — the model does not need the full rationale
    /// every turn.
    pub fn render_prompt_block(&self) -> String {
        let mut out = String::from("## Plan\n\n");
        for s in &self.steps {
            let marker = match s.status {
                PlanStatus::Done => "✓",
                PlanStatus::InProgress => "→",
                PlanStatus::Blocked => "!",
                PlanStatus::Skipped => "·",
                PlanStatus::Pending => " ",
            };
            out.push_str(&format!("{} {}. {}\n", marker, s.id + 1, s.text));
            if let Some(notes) = self.notes.get(&s.id) {
                for n in notes {
                    out.push_str(&format!("   note: {n}\n"));
                }
            }
        }
        if let Some(cur) = self.current_step() {
            out.push_str(&format!("\nCurrent step: {}. {}\n", cur.id + 1, cur.text));
        }
        out
    }
}

/// The update the model emits via the `plan_update` tool. Only one
/// variant is active per call.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum PlanUpdate {
    Advance,
    Annotate { step_id: u32, note: String },
    Insert { after_id: u32, text: String },
    Remove { step_id: u32 },
    Replace { step_id: u32, text: String },
    SetStatus { step_id: u32, status: PlanStatus },
}

impl Plan {
    /// Apply an update. Returns a human-readable description of what
    /// happened, for the tool result.
    pub fn apply(&mut self, update: PlanUpdate) -> String {
        match update {
            PlanUpdate::Advance => {
                let prev = self.current;
                self.advance();
                if prev != self.current {
                    format!("Advanced to step {}", self.current + 1)
                } else {
                    "Plan is complete".to_string()
                }
            }
            PlanUpdate::Annotate { step_id, note } => {
                self.annotate(step_id, note.clone());
                format!("Noted on step {}: {note}", step_id + 1)
            }
            PlanUpdate::Insert { after_id, text } => {
                let id = self.insert_after(after_id, text.clone());
                format!("Inserted step {} after {}", id + 1, after_id + 1)
            }
            PlanUpdate::Remove { step_id } => {
                self.remove(step_id);
                format!("Removed step {}", step_id + 1)
            }
            PlanUpdate::Replace { step_id, text } => {
                self.replace_text(step_id, text.clone());
                format!("Replaced step {} with: {text}", step_id + 1)
            }
            PlanUpdate::SetStatus { step_id, status } => {
                self.set_status(step_id, status);
                format!("Step {} now {:?}", step_id + 1, status)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_plan_starts_on_first_step() {
        let p = Plan::new("do thing", vec!["a".into(), "b".into()]);
        assert_eq!(p.current, 0);
        assert_eq!(p.current_step().unwrap().text, "a");
        assert_eq!(p.steps[0].status, PlanStatus::InProgress);
        assert_eq!(p.steps[1].status, PlanStatus::Pending);
    }

    #[test]
    fn advance_moves_to_next_pending() {
        let mut p = Plan::new("g", vec!["a".into(), "b".into()]);
        p.advance();
        assert_eq!(p.current, 1);
        assert_eq!(p.steps[0].status, PlanStatus::Done);
        assert_eq!(p.steps[1].status, PlanStatus::InProgress);
    }

    #[test]
    fn advance_off_the_end_is_a_no_op() {
        let mut p = Plan::new("g", vec!["a".into()]);
        p.advance();
        p.advance();
        p.advance();
        assert_eq!(p.current, 0);
    }

    #[test]
    fn insert_after_adds_a_step() {
        let mut p = Plan::new("g", vec!["a".into(), "b".into()]);
        let id = p.insert_after(0, "a.5".into());
        assert_eq!(p.steps.len(), 3);
        assert_eq!(p.steps[1].id, id);
    }

    #[test]
    fn remove_drops_the_step() {
        let mut p = Plan::new("g", vec!["a".into(), "b".into()]);
        p.remove(0);
        assert_eq!(p.steps.len(), 1);
        assert_eq!(p.steps[0].text, "b");
    }

    #[test]
    fn replace_changes_text() {
        let mut p = Plan::new("g", vec!["old".into()]);
        p.replace_text(0, "new".into());
        assert_eq!(p.steps[0].text, "new");
    }

    #[test]
    fn annotate_accumulates_notes() {
        let mut p = Plan::new("g", vec!["a".into()]);
        p.annotate(0, "first".into());
        p.annotate(0, "second".into());
        assert_eq!(p.notes.get(&0).unwrap().len(), 2);
    }

    #[test]
    fn progress_reports_completed_fraction() {
        let mut p = Plan::new("g", vec!["a".into(), "b".into(), "c".into()]);
        assert_eq!(p.progress(), 0.0);
        p.set_status(0, PlanStatus::Done);
        let f = p.progress();
        assert!((f - 1.0 / 3.0).abs() < 1e-6);
        p.set_status(1, PlanStatus::Skipped);
        p.set_status(2, PlanStatus::Done);
        assert_eq!(p.progress(), 1.0);
    }

    #[test]
    fn prompt_block_lists_every_step() {
        let p = Plan::new("g", vec!["a".into(), "b".into()]);
        let b = p.render_prompt_block();
        assert!(b.contains("## Plan"));
        assert!(b.contains("a"));
        assert!(b.contains("b"));
        assert!(b.contains("Current step"));
    }

    #[test]
    fn apply_advance_returns_a_description() {
        let mut p = Plan::new("g", vec!["a".into(), "b".into()]);
        let desc = p.apply(PlanUpdate::Advance);
        assert!(desc.contains("Advanced"));
    }

    #[test]
    fn apply_annotate_records_note() {
        let mut p = Plan::new("g", vec!["a".into()]);
        p.apply(PlanUpdate::Annotate {
            step_id: 0,
            note: "careful".into(),
        });
        assert_eq!(p.notes.get(&0).unwrap()[0], "careful");
    }

    #[test]
    fn apply_insert_and_remove() {
        let mut p = Plan::new("g", vec!["a".into()]);
        let desc = p.apply(PlanUpdate::Insert {
            after_id: 0,
            text: "extra".into(),
        });
        assert!(desc.contains("Inserted"));
        assert_eq!(p.steps.len(), 2);
        p.apply(PlanUpdate::Remove { step_id: 1 });
        assert_eq!(p.steps.len(), 1);
    }

    #[test]
    fn round_trip_through_json() {
        let p = Plan::new("g", vec!["a".into(), "b".into()]);
        let s = serde_json::to_string(&p).unwrap();
        let back: Plan = serde_json::from_str(&s).unwrap();
        assert_eq!(back.goal, "g");
        assert_eq!(back.steps.len(), 2);
    }

    #[test]
    fn plan_status_round_trips() {
        for s in [
            PlanStatus::Pending,
            PlanStatus::InProgress,
            PlanStatus::Done,
            PlanStatus::Blocked,
            PlanStatus::Skipped,
        ] {
            let j = serde_json::to_string(&s).unwrap();
            let back: PlanStatus = serde_json::from_str(&j).unwrap();
            assert_eq!(s, back);
        }
    }
}
