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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileDigest {
    pub path: PathBuf,
    pub summary: String,
    #[serde(default)]
    pub line_count: u32,
}

/// What a subagent hands back.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
    if let Some(start) = trimmed.find('{')
        && let Some(end) = trimmed.rfind('}')
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
            let rest = if rest.first() == Some(&'/') { &rest[1..] } else { rest };
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
            files_touched: vec![
                PathBuf::from("src/lib.rs"),
                PathBuf::from("docs/README.md"),
            ],
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
