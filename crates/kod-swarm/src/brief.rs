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
    fn globstar_matches_across_segments() {
        assert!(simple_glob_match("src/**", "src/a/b.rs"));
        assert!(simple_glob_match("**/lib.rs", "src/lib.rs"));
        assert!(simple_glob_match("**/lib.rs", "lib.rs"));
        assert!(!simple_glob_match("src/**", "docs/x.rs"));
    }
}
