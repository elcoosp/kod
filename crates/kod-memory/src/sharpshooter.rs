//! Sharpshooter: friction-gated decision memory (borrow from oh-my-pi,
//! delta §12.3).
//!
//! # The idea
//!
//! Durable project decisions are worth remembering — "we chose SQLite
//! over Postgres because the deploy target has no managed DB". But an
//! extractor asked to find decisions finds *everything*, most of it
//! restating the task. Sharpshooter's gate is **friction**: a decision
//! is worth storing when it was *earned* — a correction, a regression,
//! or a subtlety the user had to point out.
//!
//! # The admission gate
//!
//! Two checks, both cheap:
//!
//! 1. **Grounding.** The extracted `evidence` string must appear
//!    verbatim in the user's prompt. A model that invented the
//!    evidence has invented the decision; the exact-substring check
//!    drops it before it reaches the store.
//! 2. **Eligibility.** The prompt must be substantive (≥ 16 chars)
//!    and not a slash command. "ok" and "/help" are not decisions.
//!
//! # The friction flags
//!
//! [`Friction`] carries the three signals the design names. A caller
//! uses them to rank: a decision that followed a regression outranks
//! one that followed a passing comment.
//!
//! # What this is NOT
//!
//! * Not the extractor. A small model produces [`DecisionDelta`]s;
//!   this module decides whether they are admissible.
//! * Not the consolidation. Rewriting `architecture.md` under a line
//!   ceiling is a separate pass.

/// What kind of decision a delta records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionKind {
    Architecture,
    Product,
    Style,
    Constraint,
    /// An approach that was tried and rejected — worth as much as the
    /// one that was chosen.
    RejectedApproach,
    Correction,
}

impl DecisionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Architecture => "architecture_decision",
            Self::Product => "product_decision",
            Self::Style => "style_decision",
            Self::Constraint => "constraint",
            Self::RejectedApproach => "rejected_approach",
            Self::Correction => "correction",
        }
    }

    /// The file a decision of this kind belongs in.
    pub fn target_file(self) -> &'static str {
        match self {
            Self::Architecture | Self::Constraint | Self::RejectedApproach => {
                "architecture.md"
            }
            Self::Product => "product.md",
            Self::Style => "style.md",
            Self::Correction => "architecture.md",
        }
    }
}

/// The friction signals: what made the decision earn its place.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Friction {
    /// The user had to correct the agent.
    pub corrective: bool,
    /// Something regressed.
    pub regression: bool,
    /// A subtlety the user pointed out.
    pub subtle: bool,
}

impl Friction {
    /// Whether any friction fired. A decision with no friction is
    /// admissible but ranked lowest.
    pub fn any(&self) -> bool {
        self.corrective || self.regression || self.subtle
    }

    /// A rank for ordering: regression > corrective > subtle > none.
    pub fn rank(&self) -> u8 {
        if self.regression {
            3
        } else if self.corrective {
            2
        } else if self.subtle {
            1
        } else {
            0
        }
    }
}

/// One extracted decision.
#[derive(Debug, Clone, PartialEq)]
pub struct DecisionDelta {
    pub kind: DecisionKind,
    /// The decision, phrased as a timeless norm (no task state, no
    /// paths).
    pub statement: String,
    /// The alternative that was not chosen, when there was one.
    pub rejected_alternative: Option<String>,
    pub rationale: Option<String>,
    /// The exact substring of the user's prompt this came from.
    pub evidence: String,
    pub friction: Friction,
}

/// The minimum prompt length, in characters, for a prompt to be worth
/// extracting decisions from.
pub const MIN_PROMPT_CHARS: usize = 16;

/// Whether a prompt is eligible for decision extraction.
///
/// False for a short prompt (< [`MIN_PROMPT_CHARS`] chars, after
/// trimming) and for a slash command (`/`-prefixed). A slash command
/// is a harness directive, not a statement of intent.
pub fn prompt_is_eligible(prompt: &str) -> bool {
    let t = prompt.trim();
    if t.len() < MIN_PROMPT_CHARS {
        return false;
    }
    if t.starts_with('/') {
        return false;
    }
    true
}

/// Whether `evidence` is grounded in `prompt` — an exact substring,
/// case-insensitive.
///
/// The design's rule: a hallucinated decision is dropped because its
/// evidence cannot be found in the prompt. Case-insensitivity is the
/// one relaxation — a model quoting the user's words in different case
/// is not hallucinating.
pub fn evidence_is_grounded(evidence: &str, prompt: &str) -> bool {
    let e = evidence.trim();
    if e.is_empty() {
        return false;
    }
    prompt.to_lowercase().contains(&e.to_lowercase())
}

/// Admit a delta against a prompt, returning `None` when it is
/// inadmissible.
///
/// The gate runs in order: an ineligible prompt rejects everything; an
/// ungrounded evidence string rejects the delta.
pub fn admit<'a>(delta: &'a DecisionDelta, prompt: &str) -> Option<&'a DecisionDelta> {
    if !prompt_is_eligible(prompt) {
        return None;
    }
    if !evidence_is_grounded(&delta.evidence, prompt) {
        return None;
    }
    Some(delta)
}

/// Sort admitted deltas by friction rank, highest first. A stable sort
/// keeps extraction order within a rank.
pub fn rank_by_friction(mut deltas: Vec<DecisionDelta>) -> Vec<DecisionDelta> {
    deltas.sort_by_key(|d| std::cmp::Reverse(d.friction.rank()));
    deltas
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delta(evidence: &str) -> DecisionDelta {
        DecisionDelta {
            kind: DecisionKind::Architecture,
            statement: "use SQLite not Postgres".to_string(),
            rejected_alternative: Some("Postgres".to_string()),
            rationale: Some("no managed DB on the target".to_string()),
            evidence: evidence.to_string(),
            friction: Friction { corrective: true, ..Default::default() },
        }
    }

    // ---- prompt_is_eligible ------------------------------------------

    #[test]
    fn a_substantive_prompt_is_eligible() {
        assert!(prompt_is_eligible("we should use SQLite for the store"));
    }

    #[test]
    fn a_short_prompt_is_not_eligible() {
        assert!(!prompt_is_eligible("ok"));
        assert!(!prompt_is_eligible("   yes   "));
    }

    #[test]
    fn a_slash_command_is_not_eligible() {
        assert!(!prompt_is_eligible("/help me with the database choice"));
    }

    #[test]
    fn the_minimum_length_is_sixteen() {
        // 15 chars: not eligible. 16: eligible.
        assert!(!prompt_is_eligible("123456789012345"));
        assert!(prompt_is_eligible("1234567890123456"));
    }

    // ---- evidence_is_grounded ----------------------------------------

    #[test]
    fn a_verbatim_evidence_is_grounded() {
        assert!(evidence_is_grounded(
            "use SQLite",
            "I think we should use SQLite for this",
        ));
    }

    #[test]
    fn grounding_is_case_insensitive() {
        assert!(evidence_is_grounded("USE SQLITE", "we use sqlite here"));
    }

    #[test]
    fn invented_evidence_is_not_grounded() {
        assert!(!evidence_is_grounded(
            "use Postgres",
            "I think we should use SQLite for this",
        ));
    }

    #[test]
    fn empty_evidence_is_not_grounded() {
        assert!(!evidence_is_grounded("", "anything"));
        assert!(!evidence_is_grounded("   ", "anything"));
    }

    // ---- admit -------------------------------------------------------

    #[test]
    fn a_grounded_delta_in_a_substantive_prompt_is_admitted() {
        let d = delta("use SQLite");
        assert!(admit(&d, "we should use SQLite for the store").is_some());
    }

    #[test]
    fn an_ineligible_prompt_rejects_every_delta() {
        let d = delta("ok");
        assert!(admit(&d, "ok").is_none());
    }

    #[test]
    fn a_hallucinated_delta_is_rejected() {
        let d = delta("use Postgres");
        assert!(admit(&d, "we should use SQLite for the store").is_none());
    }

    // ---- friction ----------------------------------------------------

    #[test]
    fn no_friction_ranks_lowest() {
        assert_eq!(Friction::default().rank(), 0);
        assert!(!Friction::default().any());
    }

    #[test]
    fn regression_outranks_corrective_outranks_subtle() {
        let r = Friction { regression: true, ..Default::default() };
        let c = Friction { corrective: true, ..Default::default() };
        let s = Friction { subtle: true, ..Default::default() };
        assert!(r.rank() > c.rank());
        assert!(c.rank() > s.rank());
        assert!(s.rank() > Friction::default().rank());
    }

    #[test]
    fn any_is_true_for_each_flag() {
        assert!(Friction { corrective: true, ..Default::default() }.any());
        assert!(Friction { regression: true, ..Default::default() }.any());
        assert!(Friction { subtle: true, ..Default::default() }.any());
    }

    // ---- ranking -----------------------------------------------------

    #[test]
    fn rank_by_friction_orders_highest_first() {
        let mut a = delta("a");
        a.friction = Friction { subtle: true, ..Default::default() };
        let mut b = delta("b");
        b.friction = Friction { regression: true, ..Default::default() };
        let mut c = delta("c");
        c.friction = Friction::default();
        let ranked = rank_by_friction(vec![a, b, c]);
        assert_eq!(ranked[0].evidence, "b");
        assert_eq!(ranked[1].evidence, "a");
        assert_eq!(ranked[2].evidence, "c");
    }

    // ---- kinds -------------------------------------------------------

    #[test]
    fn kinds_map_to_files() {
        assert_eq!(DecisionKind::Architecture.target_file(), "architecture.md");
        assert_eq!(DecisionKind::Product.target_file(), "product.md");
        assert_eq!(DecisionKind::Style.target_file(), "style.md");
    }

    #[test]
    fn kinds_have_the_documented_spelling() {
        assert_eq!(DecisionKind::RejectedApproach.as_str(), "rejected_approach");
        assert_eq!(DecisionKind::Architecture.as_str(), "architecture_decision");
    }
}
