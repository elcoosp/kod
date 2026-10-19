//! Typed subagent briefs and reports (P5).
//!
//! The doc blames subagent mediocrity on state passing: what goes in
//! (the brief) and what comes back (the report). kod already solves
//! the write half. The read half is a one-paragraph brief plus a
//! static role preamble, with results concatenated post-hoc.
//!
//! This module is the typed contract: what a brief carries and what
//! a report returns. The assembler and merger land in follow-up
//! commits.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// What a parent hands a subagent.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ContextBrief {
    pub goal: String,
    #[serde(default)]
    pub constraints: Vec<String>,
    #[serde(default)]
    pub relevant_decisions: Vec<String>,
    #[serde(default)]
    pub file_digests: Vec<FileDigest>,
    #[serde(default)]
    pub repomap_slice: String,
    #[serde(default)]
    pub expected_writes: Vec<String>,
    #[serde(default)]
    pub token_budget: u32,
    /// Delta §11.10: the parent's approved plan, rendered as
    /// markdown. Injected when plan mode had a plan approved and the
    /// runner is dispatching a wave; skipped during plan mode itself
    /// so a draft never leaks to a subagent as if it were approved.
    #[serde(default)]
    pub plan_text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileDigest {
    pub path: PathBuf,
    pub summary: String,
    #[serde(default)]
    pub line_count: u32,
}

/// What a subagent hands back.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentReport {
    pub summary: String,
    #[serde(default)]
    pub facts_learned: Vec<String>,
    #[serde(default)]
    pub files_touched: Vec<PathBuf>,
    #[serde(default)]
    pub decisions_proposed: Vec<String>,
    #[serde(default)]
    pub open_questions: Vec<String>,
}

impl SubagentReport {
    /// Files touched outside the brief's `expected_writes` globs.
    pub fn boundary_violations(&self, brief: &ContextBrief) -> Vec<PathBuf> {
        if brief.expected_writes.is_empty() {
            return Vec::new();
        }
        self.files_touched
            .iter()
            .filter(|p| {
                let s = p.to_string_lossy();
                !brief
                    .expected_writes
                    .iter()
                    .any(|g| simple_glob_match(g, &s))
            })
            .cloned()
            .collect()
    }
}

/// Render a `ContextBrief` into the text prompt a subagent sees.
///
/// The sections are ordered by what the agent needs first: the goal
/// (what it must do), then constraints (how), then the parent's
/// state that saves it from rediscovery. A section with no entries
/// is omitted rather than rendered empty.
///
/// This is the inverse of nothing — the brief is a struct, not a
/// parseable format. The renderer is free to reformat; a caller
/// that wants to round-trip parses the subagent's report, not the
/// brief text.
pub fn render_brief(brief: &ContextBrief) -> String {
    let mut out = String::new();

    out.push_str("## Goal\n\n");
    out.push_str(brief.goal.trim());
    out.push_str("\n\n");

    if !brief.constraints.is_empty() {
        out.push_str("## Constraints\n\n");
        for c in &brief.constraints {
            out.push_str(&format!("- {}\n", c.trim()));
        }
        out.push('\n');
    }

    if !brief.relevant_decisions.is_empty() {
        out.push_str("## Decisions the parent has already made\n\n");
        out.push_str("Do not re-litigate these; respect them unless you find a concrete reason they are wrong.\n\n");
        for d in &brief.relevant_decisions {
            out.push_str(&format!("- {}\n", d.trim()));
        }
        out.push('\n');
    }

    if !brief.file_digests.is_empty() {
        out.push_str("## Files you may need\n\n");
        for d in &brief.file_digests {
            out.push_str(&format!(
                "- `{}` ({} lines): {}\n",
                d.path.display(),
                d.line_count,
                d.summary.trim()
            ));
        }
        out.push('\n');
    }

    if !brief.plan_text.trim().is_empty() {
        out.push_str("## Approved plan\n\n");
        out.push_str(
            "The parent approved this plan before dispatching you. Stay \
             within it unless you find a concrete reason it is wrong.\n\n",
        );
        out.push_str(brief.plan_text.trim());
        out.push_str("\n\n");
    }

    if !brief.repomap_slice.trim().is_empty() {
        out.push_str("## Repository map\n\n");
        out.push_str(brief.repomap_slice.trim());
        out.push_str("\n\n");
    }

    if !brief.expected_writes.is_empty() {
        out.push_str("## Write scope\n\n");
        out.push_str("Your writes must stay within these globs:\n");
        for g in &brief.expected_writes {
            out.push_str(&format!("- `{}`\n", g));
        }
        out.push('\n');
    }

    // The response contract: the runner parses a JSON report. A
    // subagent that returns prose still works (the runner falls back
    // to a bare summary), but a structured response feeds the
    // decisions log and the steers queue, so ask for it.
    out.push_str("## Response format\n\n");
    out.push_str(
        "When you finish, respond with a single JSON object:\n\n         ```json\n         {\n  \"summary\": \"one paragraph on what you did\",\n  \"facts_learned\": [\"a durable fact the parent should remember\"],\n  \"files_touched\": [\"src/lib.rs\"],\n  \"decisions_proposed\": [\"a decision for the parent to adopt or reject\"],\n  \"open_questions\": [\"something you could not resolve\"]\n}\n         ```\n\n         Every field is optional except `summary`. If you return prose instead, the runner records it as a summary and nothing else.",
    );

    out
}

/// Parse a subagent's response into a `SubagentReport`.
///
/// Lenient: strips a Markdown code fence if present, accepts a bare
/// JSON object, and falls back to a summary-only report for a
/// response that is not JSON at all — a subagent's prose is never
/// lost, only its structure is.
pub fn parse_report(text: &str) -> SubagentReport {
    let trimmed = text.trim();
    // Strip a fenced code block if the model wrapped its JSON.
    let candidate = if let Some(start) = trimmed.find("```") {
        let after = &trimmed[start + 3..];
        // Skip an optional language tag on the fence line.
        let after = match after.find('\n') {
            Some(nl) => &after[nl + 1..],
            None => after,
        };
        match after.find("```") {
            Some(end) => after[..end].trim(),
            None => after.trim(),
        }
    } else {
        trimmed
    };
    if let Ok(r) = serde_json::from_str::<SubagentReport>(candidate) {
        return r;
    }
    // Try to find a JSON object anywhere in the text.
    //
    // `start <= end` is the guard. The previous shape sliced
    // `&trimmed[start..=end]` unconditionally; an input with a `}`
    // before the first `{` (e.g. `"a}b{c"`) produced `&trimmed[3..=1]`
    // → `&trimmed[3..2]` and panicked with "byte range starts at N
    // but ends at N-1". Model-authored text with this shape is the
    // input a subagent's closing prose can produce; a panic there
    // killed the whole swarm wave.
    if let Some(start) = trimmed.find('{')
        && let Some(end) = trimmed.rfind('}')
        && start <= end
        && let Ok(r) = serde_json::from_str::<SubagentReport>(&trimmed[start..=end])
    {
        return r;
    }
    // Fallback: the whole text is the summary.
    SubagentReport {
        summary: trimmed.to_string(),
        ..Default::default()
    }
}

/// Minimal glob matcher (`*` within a segment, `**` across, `?` one
/// char).
fn simple_glob_match(pattern: &str, text: &str) -> bool {
    fn go(p: &[char], t: &[char]) -> bool {
        if p.len() >= 2 && p[0] == '*' && p[1] == '*' {
            let rest = &p[2..];
            let rest = if rest.first() == Some(&'/') {
                &rest[1..]
            } else {
                rest
            };
            for i in 0..=t.len() {
                if go(rest, &t[i..]) {
                    return true;
                }
                if i < t.len() && t[i] == '/' && go(rest, &t[i + 1..]) {
                    return true;
                }
            }
            return false;
        }
        match (p.first(), t.first()) {
            (Some('*'), _) => go(&p[1..], t) || (!t.is_empty() && go(p, &t[1..])),
            (Some('?'), Some(_)) => go(&p[1..], &t[1..]),
            (Some(a), Some(b)) if a == b => go(&p[1..], &t[1..]),
            (None, None) => true,
            _ => false,
        }
    }
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    go(&p, &t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_brief_injects_the_plan_when_present() {
        let mut b = ContextBrief {
            goal: "g".into(),
            constraints: vec![],
            relevant_decisions: vec![],
            file_digests: vec![],
            repomap_slice: String::new(),
            expected_writes: vec![],
            token_budget: 0,
            plan_text: "## Plan\n\n1. do the thing".into(),
        };
        let out = render_brief(&b);
        assert!(out.contains("## Approved plan"), "got: {out}");
        assert!(out.contains("do the thing"), "got: {out}");
        b.plan_text.clear();
        let out = render_brief(&b);
        assert!(!out.contains("## Approved plan"), "got: {out}");
    }

    #[test]
    fn empty_brief_roundtrips() {
        let b = ContextBrief::default();
        let s = serde_json::to_string(&b).unwrap();
        let back: ContextBrief = serde_json::from_str(&s).unwrap();
        assert!(back.goal.is_empty());
    }

    #[test]
    fn brief_with_every_field_roundtrips() {
        let b = ContextBrief {
            goal: "add a cache".to_string(),
            constraints: vec!["no new deps".to_string()],
            relevant_decisions: vec!["use redb".to_string()],
            file_digests: vec![FileDigest {
                path: PathBuf::from("src/lib.rs"),
                summary: "the crate root".to_string(),
                line_count: 42,
            }],
            repomap_slice: "src/lib.rs: fn main".to_string(),
            expected_writes: vec!["src/**".to_string()],
            token_budget: 4096,
            plan_text: String::new(),
        };
        let s = serde_json::to_string(&b).unwrap();
        let back: ContextBrief = serde_json::from_str(&s).unwrap();
        assert_eq!(back.goal, b.goal);
        assert_eq!(back.expected_writes, b.expected_writes);
    }

    #[test]
    fn report_roundtrips() {
        let r = SubagentReport {
            summary: "done".to_string(),
            facts_learned: vec!["redb is fast".to_string()],
            files_touched: vec![PathBuf::from("src/lib.rs")],
            decisions_proposed: vec!["use redb".to_string()],
            open_questions: vec!["which TTL?".to_string()],
        };
        let s = serde_json::to_string(&r).unwrap();
        let back: SubagentReport = serde_json::from_str(&s).unwrap();
        assert_eq!(back.summary, "done");
        assert_eq!(back.files_touched.len(), 1);
    }

    #[test]
    fn no_boundary_means_no_violations() {
        let brief = ContextBrief::default();
        let report = SubagentReport {
            files_touched: vec![PathBuf::from("anywhere")],
            ..Default::default()
        };
        assert!(report.boundary_violations(&brief).is_empty());
    }

    #[test]
    fn a_file_outside_the_declared_globs_is_a_violation() {
        let brief = ContextBrief {
            expected_writes: vec!["src/**".to_string()],
            ..Default::default()
        };
        let report = SubagentReport {
            files_touched: vec![PathBuf::from("src/lib.rs"), PathBuf::from("docs/README.md")],
            ..Default::default()
        };
        let v = report.boundary_violations(&brief);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0], PathBuf::from("docs/README.md"));
    }

    #[test]
    fn render_brief_emits_goal_and_constraints() {
        let b = ContextBrief {
            goal: "add a cache".to_string(),
            constraints: vec!["no new deps".to_string()],
            ..Default::default()
        };
        let text = render_brief(&b);
        assert!(text.contains("## Goal"));
        assert!(text.contains("add a cache"));
        assert!(text.contains("## Constraints"));
        assert!(text.contains("no new deps"));
    }

    #[test]
    fn render_brief_omits_empty_sections() {
        let b = ContextBrief {
            goal: "x".to_string(),
            ..Default::default()
        };
        let text = render_brief(&b);
        assert!(text.contains("## Goal"));
        assert!(!text.contains("## Constraints"));
        assert!(!text.contains("## Write scope"));
    }

    #[test]
    fn render_brief_includes_write_scope() {
        let b = ContextBrief {
            goal: "x".to_string(),
            expected_writes: vec!["src/**".to_string()],
            ..Default::default()
        };
        let text = render_brief(&b);
        assert!(text.contains("## Write scope"));
        assert!(text.contains("src/**"));
    }

    #[test]
    fn parse_report_accepts_bare_json() {
        let text = r#"{"summary":"done","facts_learned":["a"]}"#;
        let r = parse_report(text);
        assert_eq!(r.summary, "done");
        assert_eq!(r.facts_learned, vec!["a"]);
    }

    #[test]
    fn parse_report_strips_a_json_fence() {
        let text = "Here you go:\n\n```json\n{\"summary\":\"done\"}\n```\n";
        let r = parse_report(text);
        assert_eq!(r.summary, "done");
    }

    #[test]
    fn parse_report_falls_back_to_prose() {
        let text = "I fixed the bug but could not resolve the timeout.";
        let r = parse_report(text);
        assert!(r.summary.contains("fixed the bug"));
        assert!(r.facts_learned.is_empty());
    }

    #[test]
    fn globstar_matches_across_segments() {
        assert!(simple_glob_match("src/**", "src/a/b.rs"));
        assert!(simple_glob_match("**/lib.rs", "src/lib.rs"));
        assert!(simple_glob_match("**/lib.rs", "lib.rs"));
        assert!(!simple_glob_match("src/**", "docs/x.rs"));
    }
}

#[cfg(test)]
mod proptests {
    //! Properties of `parse_report`.
    //!
    //! `parse_report` parses the LLM subagent's report — the final
    //! text a subagent hands back to the swarm runner. The text is
    //! model-authored prose that *may* contain a JSON object; the
    //! parser tries a fenced block, then a `{...}` slice, then falls
    //! back to treating the whole text as the summary.
    //!
    //! The contract:
    //!
    //! - never panics on arbitrary input (the text is model-authored
    //!   and the swarm runner is mid-turn — a panic there kills the
    //!   whole wave);
    //! - a well-formed report round-trips through `serde_json`;
    //! - input with no `{` (so the JSON branches cannot run) is
    //!   preserved verbatim in the fallback's summary, trimmed;
    //! - a fenced JSON block parses identically to the bare JSON.
    //!
    //! The failing-first property is `prop_parse_report_never_panics`:
    //! the second branch (`trimmed.find('{')` / `trimmed.rfind('}')`)
    //! slices `&trimmed[start..=end]` without checking `start <= end`.
    //! Inputs where a `}` precedes the first `{` by more than one byte
    //! — e.g. `"a}b{c"` — panicked on the pre-fix code.
    use super::*;
    use proptest::prelude::*;

    fn arb_report() -> impl Strategy<Value = SubagentReport> {
        (
            "[a-zA-Z0-9 ,.!?]{0,60}",
            prop::collection::vec("[a-zA-Z0-9 .]{0,30}", 0..3),
            prop::collection::vec("[a-zA-Z0-9/._-]{0,30}", 0..3),
            prop::collection::vec("[a-zA-Z0-9 .]{0,30}", 0..3),
            prop::collection::vec("[a-zA-Z0-9 ?]{0,30}", 0..3),
        )
            .prop_map(|(summary, facts, files, decisions, questions)| SubagentReport {
                summary,
                facts_learned: facts,
                files_touched: files.into_iter().map(PathBuf::from).collect(),
                decisions_proposed: decisions,
                open_questions: questions,
            })
    }

    fn arb_valid_json() -> impl Strategy<Value = String> {
        arb_report().prop_map(|r| serde_json::to_string(&r).unwrap())
    }

    /// Adversarial inputs: the specific panic case, plus the shapes
    /// that exercise each branch of the parser.
    fn arb_line() -> impl Strategy<Value = String> {
        prop_oneof![
            // The pre-fix panic: a `}` before the first `{`, with a
            // gap that makes `[start..=end]` a negative-length slice.
            Just("a}b{c".to_string()),
            Just("}} {{".to_string()),
            Just("}}}}abc{".to_string()),
            // Empty / minimal.
            Just(String::new()),
            Just("{}".to_string()),
            Just("}{".to_string()),
            // Fenced forms.
            Just("```json\n{}\n```".to_string()),
            Just("prefix\n```\n{}\n```\nsuffix".to_string()),
            // Well-formed JSON (valid round-trip direction).
            arb_valid_json(),
            // Arbitrary bytes.
            any::<String>(),
        ]
    }

    proptest! {
        /// Never panics. The failing-first property: `"a}b{c"` (and
        /// every input shaped like it) panicked on the pre-fix code
        /// because the second branch sliced
        /// `&trimmed[start..=end]` without checking `start <= end`.
        #[test]
        fn prop_parse_report_never_panics(text in arb_line()) {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = parse_report(&text);
            }));
            prop_assert!(
                outcome.is_ok(),
                "parse_report panicked on a {}-byte input",
                text.len(),
            );
        }

        /// A well-formed report serializes, then parses back to the
        /// same report. The happy-path round-trip contract. A
        /// regression that dropped a field (`facts_learned`) from
        /// either side would fail on the first case with a non-empty
        /// vector.
        #[test]
        fn prop_valid_report_round_trips(r in arb_report()) {
            let json = serde_json::to_string(&r).unwrap();
            let back = parse_report(&json);
            prop_assert_eq!(back, r);
        }

        /// Fallback preserves trimmed input when the JSON branches
        /// cannot fire. With no `{` in the input, `find('{')` returns
        /// `None`, the second branch short-circuits, and the fallback
        /// sets `summary = trimmed.to_string()`. Every other field is
        /// default. A regression that truncated the summary, dropped
        /// the fallback, or defaulted the summary instead would fail.
        ///
        /// Non-vacuity: `any::<String>()` rarely contains `{`, so the
        /// filter's reject rate is low.
        #[test]
        fn prop_fallback_preserves_trimmed_input(text in any::<String>().prop_filter(
            "no open brace",
            |s| !s.contains('{'),
        )) {
            let r = parse_report(&text);
            prop_assert_eq!(&r.summary, text.trim());
            prop_assert!(r.facts_learned.is_empty());
            prop_assert!(r.files_touched.is_empty());
            prop_assert!(r.decisions_proposed.is_empty());
            prop_assert!(r.open_questions.is_empty());
        }

        /// A fenced JSON block parses identically to the bare JSON.
        /// The fence-stripping branch (find the first ```, skip an
        /// optional language tag on its line, slice to the closing
        /// ```) must not change which JSON is parsed. A regression
        /// that returned the fence text itself, or sliced the wrong
        /// end of the block, would fail.
        #[test]
        fn prop_fenced_block_strips_to_bare_json(json in arb_valid_json()) {
            let bare = parse_report(&json);
            let fenced = parse_report(&format!(
                "prefix\n```json\n{json}\n```\nsuffix",
            ));
            prop_assert_eq!(fenced, bare);
        }
    }
}
