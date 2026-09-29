//! Prewalk: one-way model handoff mid-session (borrow from oh-my-pi,
//! delta §11.12).
//!
//! # The problem
//!
//! A session started on a small/cheap model often reaches a point
//! where a stronger model is warranted — a design decision, a large
//! refactor, an unfamiliar codebase. Aborting the session to
//! restart on a different model loses the transcript; continuing
//! on the small model produces a worse plan. Prewalk is the third
//! path: keep the transcript, switch the model once, and steer the
//! next turn with a checklist the strong model can pick up from.
//!
//! # The shape
//!
//! An `Armed` prewalk carries a target model and a nudge message
//! ("think hard about the approach before acting"). The nudge is
//! injected once; when the *first* workspace-mutating action fires,
//! the prewalk fires its one handoff:
//!
//! 1. Read the current transcript.
//! 2. Splice out the nudge message (a scrub, so a later turn cannot
//!    see the hint).
//! 3. Switch the ephemeral model to the target.
//! 4. Emit a hidden checklist as the next user message.
//!
//! The one-way rule: a prewalk fires once per session. After it has
//! fired, the state is `Done` and further mutations do not re-arm.
//! A caller that wants a second handoff — a downhill walk, or a
//! prewalk back to the small model after a hard planning phase —
//! arms a new prewalk explicitly.
//!
//! # What this is NOT
//!
//! * Not the model switch. `Engine::set_current_model` does that;
//!   this module decides *when*.
//! * Not the nudge. The caller provides the message; this module
//!   tracks the state machine and the splice point.

/// The prewalk state machine.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum PrewalkState {
    /// Nothing armed; a mutation fires no handoff.
    #[default]
    Idle,
    /// Armed, waiting for the first mutating action. The nudge
    /// message is on the transcript (the engine injected it once).
    Armed,
    /// The handoff fired: the model switched, the nudge was scrubbed,
    /// and the checklist was pushed. A second mutation does nothing.
    Done,
}

/// One armed prewalk.
#[derive(Debug, Clone)]
pub struct Prewalk {
    pub state: PrewalkState,
    /// The model to switch to when the prewalk fires.
    pub target_model: String,
    /// The nudge the engine injects on `arm`. Kept so the same
    /// engine code can find and scrub the exact message later.
    pub nudge: String,
    /// The checklist the engine pushes after switching. A human
    /// checklist the strong model uses to frame the work.
    pub checklist: String,
    /// The message id of the nudge, once it is on the transcript.
    /// `None` until the engine's inject step records it.
    pub nudge_id: Option<String>,
}

impl Default for Prewalk {
    /// The idle shape: nothing armed, empty messages. A caller that
    /// wants an armed prewalk uses [`Prewalk::arm`].
    fn default() -> Self {
        Self {
            state: PrewalkState::Idle,
            target_model: String::new(),
            nudge: String::new(),
            checklist: String::new(),
            nudge_id: None,
        }
    }
}

impl Prewalk {
    /// Arm a prewalk with a target model. The caller chooses the
    /// nudge and checklist; the defaults below are the shapes the
    /// design names.
    pub fn arm(target_model: impl Into<String>) -> Self {
        Self {
            state: PrewalkState::Armed,
            target_model: target_model.into(),
            nudge: DEFAULT_NUDGE.to_string(),
            checklist: DEFAULT_CHECKLIST.to_string(),
            nudge_id: None,
        }
    }

    /// Whether the prewalk is armed and waiting for a mutation.
    pub fn is_armed(&self) -> bool {
        matches!(self.state, PrewalkState::Armed)
    }

    /// Whether the prewalk has already fired.
    pub fn is_done(&self) -> bool {
        matches!(self.state, PrewalkState::Done)
    }

    /// Record the id of the nudge message the engine injected.
    pub fn note_nudge_id(&mut self, id: impl Into<String>) {
        self.nudge_id = Some(id.into());
    }

    /// Mark the prewalk as fired. Idempotent.
    pub fn mark_done(&mut self) {
        self.state = PrewalkState::Done;
    }

    /// Whether `tool_name` counts as the workspace-mutating action
    /// that fires the handoff. Read-only tools do not count: a
    /// session that has only read files has not committed to an
    /// approach yet, and the strong model should still be watching
    /// the reads.
    pub fn is_mutating_tool(tool_name: &str) -> bool {
        matches!(
            tool_name,
            "write_file" | "patch_file" | "edit" | "git_commit"
        )
    }
}

/// The default nudge injected when a prewalk arms.
pub const DEFAULT_NUDGE: &str = "Before you start editing, plan the approach deliberately. Name the \
     files you will change, the invariants you must preserve, and the \
     test you will run to check the change. Do not edit until you have \
     written that plan.";

/// The default checklist pushed after the handoff.
pub const DEFAULT_CHECKLIST: &str = "Checklist for this task:\n\
     1. Read the files the plan names before editing.\n\
     2. Make one focused change at a time.\n\
     3. Run the test or check the plan named.\n\
     4. If the plan is wrong, say so before editing around it.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_prewalk_is_idle() {
        let p = Prewalk::default();
        assert_eq!(p.state, PrewalkState::Idle);
        assert!(!p.is_armed());
        assert!(!p.is_done());
    }

    #[test]
    fn arming_sets_armed() {
        let p = Prewalk::arm("anthropic/claude-opus-4");
        assert!(p.is_armed());
        assert_eq!(p.target_model, "anthropic/claude-opus-4");
    }

    #[test]
    fn arming_uses_the_default_nudge_and_checklist() {
        let p = Prewalk::arm("x/y");
        assert!(p.nudge.contains("plan the approach"));
        assert!(p.checklist.contains("Checklist"));
    }

    #[test]
    fn note_nudge_id_records_it() {
        let mut p = Prewalk::arm("x/y");
        p.note_nudge_id("msg-1");
        assert_eq!(p.nudge_id.as_deref(), Some("msg-1"));
    }

    #[test]
    fn mark_done_is_one_way() {
        let mut p = Prewalk::arm("x/y");
        p.mark_done();
        assert!(p.is_done());
        assert!(!p.is_armed());
        // A second mark does not resurrect the armed state.
        p.mark_done();
        assert!(p.is_done());
    }

    #[test]
    fn mutating_tool_recognizes_writes() {
        for t in ["write_file", "patch_file", "edit", "git_commit"] {
            assert!(Prewalk::is_mutating_tool(t), "{t}");
        }
    }

    #[test]
    fn mutating_tool_rejects_reads() {
        for t in ["read_file", "grep", "list_files", "file_info", "web_search"] {
            assert!(!Prewalk::is_mutating_tool(t), "{t}");
        }
    }
}
