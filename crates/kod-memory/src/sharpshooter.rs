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
//! * Not the consolidation. Rewriting `architecture.md` under a line
//!   ceiling is a separate pass. The extraction prompt and the JSON
//!   parse of the model's reply live here; the consolidation pass and
//!   the file writes live above this module (in the engine).

use serde::{Deserialize, Serialize};

/// What kind of decision a delta records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DecisionKind {
    #[serde(rename = "architecture_decision")]
    Architecture,
    #[serde(rename = "product_decision")]
    Product,
    #[serde(rename = "style_decision")]
    Style,
    #[serde(rename = "constraint")]
    Constraint,
    /// An approach that was tried and rejected — worth as much as the
    /// one that was chosen.
    #[serde(rename = "rejected_approach")]
    RejectedApproach,
    #[serde(rename = "correction")]
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
            Self::Architecture | Self::Constraint | Self::RejectedApproach => "architecture.md",
            Self::Product => "product.md",
            Self::Style => "style.md",
            Self::Correction => "architecture.md",
        }
    }
}

/// The friction signals: what made the decision earn its place.
///
/// `#[serde(default)]` at the container level: a reply that sends
/// `{}` (or omits any of the three flags) parses to the all-false
/// default. `#[serde(default)]` on the enclosing `DecisionDelta`
/// field only fills in a *missing* object; a present-but-empty
/// object still needs every field unless the struct itself is
/// defaulted. The model does not always send all three flags.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionDelta {
    pub kind: DecisionKind,
    /// The decision, phrased as a timeless norm (no task state, no
    /// paths).
    pub statement: String,
    /// The alternative that was not chosen, when there was one.
    ///
    /// `#[serde(default)]` so a model that omits the field parses; the
    /// alternative is genuinely optional, and a strict requirement
    /// would reject well-formed deltas over a field the design calls
    /// optional.
    #[serde(default)]
    pub rejected_alternative: Option<String>,
    #[serde(default)]
    pub rationale: Option<String>,
    /// The exact substring of the user's prompt this came from.
    pub evidence: String,
    /// The friction flags. `#[serde(default)]` so a reply that omits
    /// the field — or sends `{}` — parses; the default has no friction
    /// flagged, and the admission gate treats a no-friction delta as
    /// admissible but ranked lowest.
    #[serde(default)]
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
    // `MIN_PROMPT_CHARS` names a character count, so count chars,
    // not bytes. The pre-fix code used `t.len()` (bytes), which made
    // a 15-CJK-char prompt (45 bytes) eligible while a 15-ASCII-char
    // prompt was not. For ASCII the two agree; for CJK and emoji
    // they differ by 3x and 4x. The properties
    // `prop_not_eligible_15_cjk_chars` and
    // `prop_not_eligible_4_emoji` pin the character-boundary
    // behaviour.
    if t.chars().count() < MIN_PROMPT_CHARS {
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

/// The default cap on decisions extracted from a single prompt. A
/// prompt that "contains" eight decisions almost certainly contains
/// four decisions and four restatements of the task.
pub const DEFAULT_MAX_DECISIONS: usize = 4;

/// Build the extraction prompt for one user message.
///
/// The prompt asks for a JSON array of decisions, each shaped like a
/// `DecisionDelta` (see the module doc). The `evidence` field must be
/// an exact substring of the user's prompt; the prompt tells the
/// model so explicitly, and the caller checks it before admitting a
/// delta anyway — the model's compliance is not assumed.
///
/// `max_entries` is a hard cap; the reply parser also enforces it so
/// a verbose model cannot grow the delta queue past the budget.
pub fn build_prompt(user_prompt: &str, max_entries: usize) -> String {
    let cap = max_entries.max(1);
    format!(
        "You extract durable project decisions from a single user \
         message. A decision is worth recording only when it carries \
         friction: the user corrected a mistaken assumption, something \
         regressed, or a subtlety had to be pointed out. Restating the \
         task is not a decision.\n\n\
         Return at most {cap} decision(s) as a JSON array. If there is \
         no decision, return []. Each element has this shape:\n\n\
         {{\n  \
           \"kind\": one of \"architecture_decision\", \"product_decision\", \
         \"style_decision\", \"constraint\", \"rejected_approach\", \
         \"correction\",\n  \
           \"statement\": a timeless normative sentence — no task state, \
         no file paths,\n  \
           \"rejected_alternative\": optional, the option not chosen,\n  \
           \"rationale\": optional, why,\n  \
           \"evidence\": an EXACT substring of the user message below \
         that the decision is grounded in,\n  \
           \"friction\": {{\"corrective\": bool, \"regression\": bool, \
         \"subtle\": bool}}\n\
         }}\n\n\
         The user message:\n\n{user_prompt}\n",
    )
}

/// The hard line ceiling per decision file (borrow from oh-my-pi,
/// delta §12.3). A consolidation pass that produced a longer file
/// would grow without bound across sessions; the ceiling is what
/// forces the model to *replace*, not append.
pub const FILE_LINE_CEILING: usize = 120;

/// Build the consolidation prompt for one decision file.
///
/// The prompt carries the current file's text, the deltas (in
/// friction-ranked order), and the rules that make the output usable:
/// a timeless normative statement per entry, no task state, no file
/// paths, and a hard line ceiling.
pub fn build_consolidation_prompt(existing: &str, deltas: &[DecisionDelta]) -> String {
    let existing = if existing.trim().is_empty() {
        "(empty)".to_string()
    } else {
        existing.to_string()
    };
    // Serialize deltas to JSON so the model sees a stable field
    // shape. Friction-ranked order is preserved by the caller's
    // `rank_by_friction` call.
    let deltas_json = serde_json::to_string_pretty(deltas).unwrap_or_else(|_| "[]".to_string());
    format!(
        "You consolidate a project's friction-earned decisions into a \
         single markdown document. The current document is below \
         (may be empty). The new decisions to incorporate are in the \
         JSON array, ordered by friction rank (highest first).\n\n\
         Rules:\n\
         - HARD LIMIT: at most {FILE_LINE_CEILING} lines. If the current \
           document plus the new decisions exceeds this, drop the \
           least important entries — preserve recent decisions and \
           decisions with friction.\n\
         - Preserve existing entries unless a new decision contradicts \
           one, in which case the newer decision wins.\n\
         - Group related entries under short headings.\n\
         - Each entry is a timeless normative sentence: no task state, \
           no file paths, no code snippets longer than a line.\n\
         - Return the rewritten document only. No prose, no fences.\n\n\
         Current document:\n{existing}\n\n\
         New decisions (JSON):\n{deltas_json}\n",
    )
}

/// Truncate a consolidated document to [`FILE_LINE_CEILING`] lines.
///
/// The model is *told* the ceiling; this enforces it. A reply that
/// overruns is cut at the last full line, so a partial sentence never
/// survives into the file.
pub fn truncate_to_ceiling(text: &str) -> String {
    let mut out = String::new();
    for (i, line) in text.lines().enumerate() {
        if i >= FILE_LINE_CEILING {
            break;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.trim_end().to_string()
}

/// Parse the model's reply into decisions.
///
/// The reply is expected to be a JSON array, optionally surrounded by
/// prose. The parser takes the first `[` through the last `]`, and
/// deserializes the slice. A bad reply — no array, invalid JSON, wrong
/// shape — yields an empty vector; a caller treats that as "no
/// decisions this turn" rather than an error, matching the extraction
/// path's best-effort contract.
pub fn parse_reply(reply: &str, max_entries: usize) -> Vec<DecisionDelta> {
    let Some(start) = reply.find('[') else {
        return Vec::new();
    };
    let Some(end) = reply.rfind(']') else {
        return Vec::new();
    };
    if end <= start {
        return Vec::new();
    }
    let slice = &reply[start..=end];
    let Ok(mut parsed): Result<Vec<DecisionDelta>, _> = serde_json::from_str(slice) else {
        return Vec::new();
    };
    parsed.truncate(max_entries.max(1));
    // Drop deltas with an obviously malformed shape before the caller
    // runs the full admission gate: an empty statement or empty
    // evidence is never admissible.
    parsed.retain(|d| !d.statement.trim().is_empty() && !d.evidence.trim().is_empty());
    parsed
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
            friction: Friction {
                corrective: true,
                ..Default::default()
            },
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
        let r = Friction {
            regression: true,
            ..Default::default()
        };
        let c = Friction {
            corrective: true,
            ..Default::default()
        };
        let s = Friction {
            subtle: true,
            ..Default::default()
        };
        assert!(r.rank() > c.rank());
        assert!(c.rank() > s.rank());
        assert!(s.rank() > Friction::default().rank());
    }

    #[test]
    fn any_is_true_for_each_flag() {
        assert!(
            Friction {
                corrective: true,
                ..Default::default()
            }
            .any()
        );
        assert!(
            Friction {
                regression: true,
                ..Default::default()
            }
            .any()
        );
        assert!(
            Friction {
                subtle: true,
                ..Default::default()
            }
            .any()
        );
    }

    // ---- ranking -----------------------------------------------------

    #[test]
    fn rank_by_friction_orders_highest_first() {
        let mut a = delta("a");
        a.friction = Friction {
            subtle: true,
            ..Default::default()
        };
        let mut b = delta("b");
        b.friction = Friction {
            regression: true,
            ..Default::default()
        };
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

    // ---- extraction prompt & parse -----------------------------------

    #[test]
    fn the_prompt_names_the_cap_and_the_shape() {
        let p = build_prompt("we settled on SQLite not Postgres", 3);
        assert!(p.contains("at most 3"), "got: {p}");
        assert!(p.contains("architecture_decision"), "got: {p}");
        assert!(p.contains("we settled on SQLite not Postgres"), "got: {p}");
    }

    #[test]
    fn the_prompt_asks_for_an_exact_evidence_substring() {
        let p = build_prompt("some prompt", 1);
        assert!(p.contains("EXACT substring"), "got: {p}");
    }

    #[test]
    fn a_well_formed_array_parses() {
        let reply = r#"[
            {
                "kind": "architecture_decision",
                "statement": "use SQLite, not Postgres",
                "rejected_alternative": "Postgres",
                "rationale": "no managed DB on the target",
                "evidence": "we use SQLite",
                "friction": {"corrective": false, "regression": false, "subtle": false}
            }
        ]"#;
        let out = parse_reply(reply, 4);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, DecisionKind::Architecture);
        assert_eq!(out[0].statement, "use SQLite, not Postgres");
        assert_eq!(out[0].rejected_alternative.as_deref(), Some("Postgres"));
    }

    #[test]
    fn surrounding_prose_is_ignored() {
        let reply = "Sure, here you go:\n\
            [{\"kind\":\"correction\",\"statement\":\"use cargo check\",\
             \"evidence\":\"cargo check\",\"friction\":{}}]\n\
            Done.";
        let out = parse_reply(reply, 4);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, DecisionKind::Correction);
    }

    #[test]
    fn an_empty_array_parses_to_nothing() {
        assert!(parse_reply("[]", 4).is_empty());
    }

    #[test]
    fn a_reply_without_an_array_yields_nothing() {
        assert!(parse_reply("no decisions here", 4).is_empty());
        assert!(parse_reply("", 4).is_empty());
    }

    #[test]
    fn invalid_json_yields_nothing() {
        assert!(parse_reply("[not, valid]", 4).is_empty());
    }

    #[test]
    fn parse_respects_the_cap() {
        let reply = r#"[
            {"kind":"correction","statement":"a","evidence":"e","friction":{}},
            {"kind":"correction","statement":"b","evidence":"e","friction":{}},
            {"kind":"correction","statement":"c","evidence":"e","friction":{}}
        ]"#;
        assert_eq!(parse_reply(reply, 2).len(), 2);
    }

    #[test]
    fn parse_drops_empty_statement_or_evidence() {
        let reply = r#"[
            {"kind":"correction","statement":"","evidence":"e","friction":{}},
            {"kind":"correction","statement":"a","evidence":"","friction":{}},
            {"kind":"correction","statement":"keep","evidence":"e","friction":{}}
        ]"#;
        let out = parse_reply(reply, 4);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].statement, "keep");
    }

    // ---- consolidation -----------------------------------------------

    #[test]
    fn the_prompt_names_the_ceiling_and_the_existing_text() {
        let deltas = vec![DecisionDelta {
            kind: DecisionKind::Architecture,
            statement: "use SQLite, not Postgres".to_string(),
            rejected_alternative: None,
            rationale: None,
            evidence: "we use SQLite".to_string(),
            friction: Friction::default(),
        }];
        let p = build_consolidation_prompt("## Conventions\n\n- be terse", &deltas);
        assert!(p.contains("at most 120 lines"), "got: {p}");
        assert!(p.contains("be terse"), "got: {p}");
        assert!(p.contains("use SQLite, not Postgres"), "got: {p}");
    }

    #[test]
    fn the_prompt_says_empty_when_there_is_no_existing_text() {
        let p = build_consolidation_prompt("   \n", &[]);
        assert!(p.contains("(empty)"), "got: {p}");
    }

    #[test]
    fn truncate_drops_lines_past_the_ceiling() {
        let body: String = (0..200).map(|i| format!("line {i}\n")).collect();
        let out = truncate_to_ceiling(&body);
        assert_eq!(out.lines().count(), FILE_LINE_CEILING);
        assert!(out.contains("line 119"), "got: {out}");
        assert!(!out.contains("line 120"), "got: {out}");
    }

    #[test]
    fn truncate_leaves_a_short_document_alone() {
        let body = "## A\n\n- one\n- two\n";
        assert_eq!(truncate_to_ceiling(body), "## A\n\n- one\n- two");
    }

    #[test]
    fn a_delta_round_trips_through_json() {
        let d = DecisionDelta {
            kind: DecisionKind::RejectedApproach,
            statement: "do not reach for tokio::spawn".to_string(),
            rejected_alternative: Some("tokio::spawn".to_string()),
            rationale: Some("the engine owns the task lifecycle".to_string()),
            evidence: "we don't spawn from the manager".to_string(),
            friction: Friction {
                corrective: true,
                ..Default::default()
            },
        };
        let json = serde_json::to_string(&d).unwrap();
        let back: DecisionDelta = serde_json::from_str(&json).unwrap();
        assert_eq!(back.kind, d.kind);
        assert_eq!(back.friction, d.friction);
        // The serde rename must serialize the kind as the design's
        // spelling, not the Rust variant name.
        assert!(json.contains("rejected_approach"), "got: {json}");
    }
}

#[cfg(test)]
mod sharpshooter_proptests {
    //! Properties for the eight pure functions in this module.
    //!
    //! The example tests pin specific cases. These properties pin the
    //! shape across arbitrary inputs, and one of them finds a real
    //! bug in `prompt_is_eligible`.
    //!
    //! **The bug:** `prompt_is_eligible` compares `t.len()` (bytes)
    //! against `MIN_PROMPT_CHARS` (named for chars). For ASCII the
    //! two agree, so the example test `the_minimum_length_is_sixteen`
    //! passes. For CJK and emoji they diverge by 3x and 4x:
    //!
    //! * 15 CJK chars = 45 bytes. `45 >= 16` → eligible. The intent
    //!   (16 chars minimum) says it should not be.
    //! * 4 emoji = 16 bytes. `16 >= 16` → eligible. The intent says
    //!   it should not be (4 < 16).
    //!
    //! Both properties below fail on the pre-fix code and pass after.
    //! The fix is `t.chars().count()` in place of `t.len()`.

    use super::*;
    use proptest::prelude::*;

    // ---- prompt_is_eligible ---------------------------------------------

    /// ASCII boundary: 16 chars eligible, 15 chars not. Pins the
    /// documented minimum so a change to `MIN_PROMPT_CHARS` is a
    /// visible decision.
    #[test]
    fn prop_eligible_ascii_boundary() {
        assert!(prompt_is_eligible(&"a".repeat(16)));
        assert!(!prompt_is_eligible(&"a".repeat(15)));
    }

    /// **The bug: 15 CJK chars (45 bytes) should NOT be eligible**
    /// (15 < 16 chars). The pre-fix code accepts it because
    /// `t.len()` = 45 >= 16. Post-fix `t.chars().count()` = 15 < 16.
    #[test]
    fn prop_not_eligible_15_cjk_chars() {
        let s = "中".repeat(15);
        assert_eq!(s.chars().count(), 15);
        assert_eq!(s.len(), 45, "15 CJK chars = 45 bytes");
        assert!(
            !prompt_is_eligible(&s),
            "15-char CJK prompt should not be eligible (15 < 16 chars)",
        );
    }

    /// **The bug: 4 emoji (16 bytes) should NOT be eligible**
    /// (4 < 16 chars). The pre-fix code accepts it because
    /// `t.len()` = 16 >= 16.
    #[test]
    fn prop_not_eligible_4_emoji() {
        let s = "😀😀😀😀";
        assert_eq!(s.chars().count(), 4);
        assert_eq!(s.len(), 16);
        assert!(
            !prompt_is_eligible(s),
            "4-emoji prompt should not be eligible (4 < 16 chars), \
             but the byte count 16 passes a 16-byte threshold",
        );
    }

    /// 16 CJK chars (48 bytes) IS eligible under both interpretations
    /// (16 >= 16 chars, 48 >= 16 bytes). Pins the non-buggy side of
    /// the same boundary.
    #[test]
    fn prop_eligible_16_cjk_chars() {
        let s = "中".repeat(16);
        assert_eq!(s.chars().count(), 16);
        assert_eq!(s.len(), 48, "16 CJK chars = 48 bytes");
        assert!(prompt_is_eligible(&s));
    }

    /// A slash-prefixed prompt is not eligible regardless of length.
    #[test]
    fn prop_slash_prefixed_not_eligible() {
        for n in 16..=100 {
            let s = format!("/{}", "a".repeat(n));
            assert!(
                !prompt_is_eligible(&s),
                "/-prefixed prompt of len {} should not be eligible",
                s.len()
            );
        }
    }

    // ---- evidence_is_grounded -------------------------------------------

    proptest! {
        /// Evidence that is a case-insensitive substring of the
        /// prompt is grounded. This is the whole contract.
        #[test]
        fn prop_evidence_substring_grounded(
            prefix in "[a-zA-Z ]{0,20}",
            evidence in "[a-zA-Z ]{1,20}",
            suffix in "[a-zA-Z ]{0,20}",
        ) {
            let prompt = format!("{}{}{}", prefix, evidence, suffix);
            // No additional assumption — evidence.trim() could still
            // be non-empty; if all-whitespace, the function correctly
            // returns false.
            if !evidence.trim().is_empty() {
                prop_assert!(evidence_is_grounded(&evidence, &prompt));
            }
        }

        /// Case-insensitive: uppercasing either side does not change
        /// the result (for ASCII, where `to_lowercase` is
        /// byte-preserving).
        #[test]
        fn prop_evidence_ascii_case_insensitive(
            evidence in "[a-zA-Z]{1,10}",
            prompt in "[a-zA-Z]{0,30}",
        ) {
            prop_assert_eq!(
                evidence_is_grounded(&evidence, &prompt),
                evidence_is_grounded(&evidence.to_uppercase(), &prompt),
            );
            prop_assert_eq!(
                evidence_is_grounded(&evidence, &prompt),
                evidence_is_grounded(&evidence, &prompt.to_uppercase()),
            );
        }
    }

    /// Empty or whitespace-only evidence is never grounded.
    #[test]
    fn prop_empty_evidence_not_grounded() {
        assert!(!evidence_is_grounded("", "anything"));
        assert!(!evidence_is_grounded("   ", "anything"));
        assert!(!evidence_is_grounded("\t\n", "anything"));
    }

    // ---- rank_by_friction -----------------------------------------------

    fn delta(statement: &str, friction: Friction) -> DecisionDelta {
        DecisionDelta {
            kind: DecisionKind::Correction,
            statement: statement.to_string(),
            rejected_alternative: None,
            rationale: None,
            evidence: statement.to_string(),
            friction,
        }
    }

    fn zero_friction() -> Friction {
        Friction {
            corrective: false,
            regression: false,
            subtle: false,
        }
    }

    fn corrective_only() -> Friction {
        Friction {
            corrective: true,
            regression: false,
            subtle: false,
        }
    }

    fn regression_only() -> Friction {
        Friction {
            corrective: false,
            regression: true,
            subtle: false,
        }
    }

    /// Monotone: after ranking, no delta's friction rank exceeds the
    /// rank of the one before it.
    #[test]
    fn prop_rank_monotone() {
        let deltas = vec![
            delta("a", zero_friction()),
            delta("b", regression_only()),
            delta("c", corrective_only()),
            delta("d", zero_friction()),
            delta("e", regression_only()),
        ];
        let ranked = rank_by_friction(deltas);
        for pair in ranked.windows(2) {
            assert!(
                pair[0].friction.rank() >= pair[1].friction.rank(),
                "rank order violated: {} then {}",
                pair[0].friction.rank(),
                pair[1].friction.rank(),
            );
        }
    }

    /// Stable: deltas of equal friction preserve their input order.
    #[test]
    fn prop_rank_stable_for_equal_friction() {
        let deltas = vec![
            delta("first", zero_friction()),
            delta("second", zero_friction()),
            delta("third", zero_friction()),
            delta("fourth", zero_friction()),
        ];
        let ranked = rank_by_friction(deltas);
        let statements: Vec<&str> = ranked.iter().map(|d| d.statement.as_str()).collect();
        assert_eq!(statements, vec!["first", "second", "third", "fourth"]);
    }

    /// Length preserved: ranking does not add or drop deltas.
    #[test]
    fn prop_rank_preserves_length() {
        for n in 0..=20 {
            let deltas: Vec<DecisionDelta> = (0..n)
                .map(|i| delta(&format!("d{i}"), zero_friction()))
                .collect();
            let ranked = rank_by_friction(deltas);
            assert_eq!(ranked.len(), n);
        }
    }

    // ---- truncate_to_ceiling --------------------------------------------

    /// Line count is at most `FILE_LINE_CEILING` (120).
    #[test]
    fn prop_truncate_line_count_at_most_ceiling() {
        for n in [0, 1, 50, 119, 120, 121, 200, 500] {
            let text = (0..n)
                .map(|i| format!("line {i}"))
                .collect::<Vec<_>>()
                .join("\n");
            let out = truncate_to_ceiling(&text);
            assert!(
                out.lines().count() <= FILE_LINE_CEILING,
                "input with {} lines yielded {} lines (ceiling {})",
                n,
                out.lines().count(),
                FILE_LINE_CEILING,
            );
        }
    }

    /// Idempotent: truncating twice equals truncating once.
    #[test]
    fn prop_truncate_idempotent() {
        for n in [0, 1, 120, 121, 500] {
            let text = (0..n)
                .map(|i| format!("line {i}"))
                .collect::<Vec<_>>()
                .join("\n");
            let once = truncate_to_ceiling(&text);
            let twice = truncate_to_ceiling(&once);
            assert_eq!(once, twice, "truncate not idempotent for {} lines", n);
        }
    }

    // ---- parse_reply ----------------------------------------------------

    /// An empty reply or a reply without a JSON array yields nothing.
    #[test]
    fn prop_parse_reply_empty_yields_empty() {
        for reply in ["", "no array here", "{}", "still nothing"] {
            let parsed = parse_reply(reply, 4);
            assert!(
                parsed.is_empty(),
                "reply {:?} should parse to nothing",
                reply
            );
        }
    }

    /// A well-formed JSON array round-trips: parse_reply with a
    /// generous cap returns deltas whose statements match the input.
    #[test]
    fn prop_parse_reply_well_formed_roundtrip() {
        let input = vec![
            delta("decision one", zero_friction()),
            delta("decision two", corrective_only()),
        ];
        let json = serde_json::to_string(&input).expect("Delta serializes");
        let parsed = parse_reply(&json, 10);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].statement, "decision one");
        assert_eq!(parsed[1].statement, "decision two");
    }

    /// The returned count is bounded by `max_entries.max(1)`.
    #[test]
    fn prop_parse_reply_max_entries_bound() {
        for n in 0..=5 {
            let deltas: Vec<DecisionDelta> = (0..10)
                .map(|i| delta(&format!("d{i}"), zero_friction()))
                .collect();
            let json = serde_json::to_string(&deltas).expect("serializes");
            let parsed = parse_reply(&json, n);
            assert!(
                parsed.len() <= n.max(1),
                "max_entries={} yielded {} deltas (bound {})",
                n,
                parsed.len(),
                n.max(1),
            );
        }
    }

    // ---- controls -------------------------------------------------------

    /// Control: evidence "istanbul" is **not** grounded against a
    /// prompt containing Turkish "İstanbul". `to_lowercase` expands
    /// `İ` to `i` + combining dot, so the lowercased evidence is not
    /// a byte-substring of the lowercased prompt. Documented, not
    /// fixed: Turkish (and German ß) case-folding is more than the
    /// function's `to_lowercase` handles. Pin so a future fix to a
    /// Unicode-aware matcher is a decision.
    #[test]
    fn control_turkish_i_not_grounded() {
        assert!(
            !evidence_is_grounded("istanbul", "We discussed İstanbul yesterday"),
            "current behaviour: Turkish İ expansion breaks case-insensitive grounding",
        );
    }

    /// Control: `parse_reply` truncates **before** filtering malformed
    /// entries. A reply whose first `max_entries` deltas are all
    /// malformed yields an empty vec, even if later deltas are valid.
    /// `max_entries` is applied to the parsed array, then
    /// `retain` drops empty-statement/empty-evidence entries.
    /// Defensible ("only the first N the model returned"), pinned so
    /// a future reorder (filter-then-truncate) is a visible decision.
    #[test]
    fn control_parse_reply_truncates_before_filtering() {
        let json = r#"[
            {"kind":"correction","statement":"","evidence":""},
            {"kind":"correction","statement":"","evidence":""},
            {"kind":"correction","statement":"valid one","evidence":"valid one"},
            {"kind":"correction","statement":"valid two","evidence":"valid two"}
        ]"#;
        let parsed = parse_reply(json, 2);
        assert_eq!(
            parsed.len(),
            0,
            "first 2 are malformed, cap is 2 — nothing survives"
        );
        let parsed_generous = parse_reply(json, 10);
        assert_eq!(
            parsed_generous.len(),
            2,
            "with a generous cap, both valid entries survive"
        );
    }

    /// Control: `truncate_to_ceiling` strips trailing whitespace
    /// (via `trim_end`). A 3-line input with trailing spaces on the
    /// last line yields the same text with those spaces removed.
    /// Pinned so a change to `trim_end()` (e.g. to preserve trailing
    /// whitespace) is a decision.
    #[test]
    fn control_truncate_strips_trailing_whitespace() {
        let text = "line one\nline two\nline three   \n\n";
        let out = truncate_to_ceiling(text);
        assert!(!out.ends_with(' '));
        assert!(!out.ends_with('\n'));
        assert!(out.starts_with("line one"));
    }
}
