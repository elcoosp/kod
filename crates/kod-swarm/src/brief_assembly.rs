//! Assemblers and mergers for typed subagent briefs (P5).
//!
//! [`super::brief`] defines the two records; this module fills and
//! drains them. A subagent's output is not a blob appended to the
//! parent transcript — it is a set of typed facts each routed to
//! the durable store that will use it.

use std::path::{Path, PathBuf};

use super::brief::{ContextBrief, FileDigest, SubagentReport};

/// Inputs the assembler needs from the parent.
#[derive(Debug, Clone, Default)]
pub struct ParentContext {
    pub decisions: Vec<String>,
    pub file_summaries: Vec<(PathBuf, String, u32)>,
    pub repomap_text: String,
    pub expected_writes: Vec<String>,
    pub token_budget: u32,
}

/// Build a `ContextBrief` for a subtask.
pub fn assemble_brief(
    goal: &str,
    role_preamble: &str,
    constraints: Vec<String>,
    parent: &ParentContext,
) -> ContextBrief {
    const MAX_DECISIONS: usize = 8;
    const MAX_DIGESTS: usize = 12;

    let goal_terms = tokenize(goal);

    let mut scored: Vec<(f64, &String)> = parent
        .decisions
        .iter()
        .map(|d| {
            let lower = d.to_lowercase();
            let score: f64 = goal_terms
                .iter()
                .filter(|t| lower.contains(t.as_str()))
                .count() as f64;
            (score, d)
        })
        .collect();
    scored.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let relevant_decisions: Vec<String> = scored
        .into_iter()
        .take(MAX_DECISIONS)
        .map(|(_, d)| d.clone())
        .collect();

    let file_digests: Vec<FileDigest> = parent
        .file_summaries
        .iter()
        .take(MAX_DIGESTS)
        .map(|(path, summary, lines)| FileDigest {
            path: path.clone(),
            summary: summary.clone(),
            line_count: *lines,
        })
        .collect();

    let mut all_constraints = Vec::with_capacity(constraints.len() + 1);
    if !role_preamble.trim().is_empty() {
        all_constraints.push(role_preamble.trim().to_string());
    }
    all_constraints.extend(constraints);

    ContextBrief {
        goal: goal.to_string(),
        constraints: all_constraints,
        relevant_decisions,
        file_digests,
        repomap_slice: parent.repomap_text.clone(),
        expected_writes: parent.expected_writes.clone(),
        token_budget: parent.token_budget,
    }
}

/// What a report implies for the parent, as a set of typed
/// destinations.
#[derive(Debug, Clone, Default)]
pub struct MergePlan {
    pub decisions_to_record: Vec<String>,
    pub steers_to_queue: Vec<String>,
    pub boundary_violations: Vec<PathBuf>,
    pub summary_line: String,
}

/// Compute what a report implies for the parent.
pub fn merge_report(report: &SubagentReport, brief: &ContextBrief) -> MergePlan {
    MergePlan {
        decisions_to_record: report
            .facts_learned
            .iter()
            .chain(report.decisions_proposed.iter())
            .cloned()
            .collect(),
        steers_to_queue: report.open_questions.clone(),
        boundary_violations: report.boundary_violations(brief),
        summary_line: if report.summary.trim().is_empty() {
            "(subagent returned no summary)".to_string()
        } else {
            report.summary.trim().to_string()
        },
    }
}

fn tokenize(s: &str) -> Vec<String> {
    s.split_whitespace()
        .map(|t| {
            t.trim_matches(|c: char| !c.is_alphanumeric() && c != '_')
                .to_lowercase()
        })
        .filter(|t| t.len() >= 3)
        .collect()
}

/// Read a file and produce a compact `FileDigest`.
pub fn digest_file(path: &Path) -> std::io::Result<FileDigest> {
    let body = std::fs::read_to_string(path)?;
    let line_count = body.lines().count() as u32;
    let summary = body
        .lines()
        .filter(|l| !l.trim().is_empty())
        .take(3)
        .collect::<Vec<_>>()
        .join(" ");
    Ok(FileDigest {
        path: path.to_path_buf(),
        summary: summary.chars().take(280).collect(),
        line_count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ParentContext {
        ParentContext {
            decisions: vec![
                "use redb for storage".to_string(),
                "prefer tracing over println".to_string(),
                "no new deps".to_string(),
                "the cache key includes the tool set".to_string(),
            ],
            file_summaries: vec![
                (PathBuf::from("src/lib.rs"), "crate root".to_string(), 42),
                (PathBuf::from("src/cache.rs"), "cache impl".to_string(), 88),
            ],
            repomap_text: "src/lib.rs: fn main".to_string(),
            expected_writes: vec!["src/**".to_string()],
            token_budget: 4096,
        }
    }

    #[test]
    fn empty_goal_still_produces_a_well_formed_brief() {
        let brief = assemble_brief("", "", vec![], &ctx());
        assert!(brief.goal.is_empty());
        assert_eq!(brief.expected_writes.len(), 1);
    }

    #[test]
    fn role_preamble_is_the_first_constraint() {
        let brief = assemble_brief("do x", "you are a reviewer", vec!["no deps".into()], &ctx());
        assert_eq!(brief.constraints[0], "you are a reviewer");
        assert_eq!(brief.constraints[1], "no deps");
    }

    #[test]
    fn decisions_are_scored_against_the_goal() {
        let brief = assemble_brief("implement redb caching", "", vec![], &ctx());
        assert!(
            brief
                .relevant_decisions
                .contains(&"use redb for storage".to_string())
        );
        assert!(
            brief
                .relevant_decisions
                .contains(&"the cache key includes the tool set".to_string())
        );
    }

    #[test]
    fn digests_are_capped() {
        let mut c = ctx();
        for i in 0..30 {
            c.file_summaries
                .push((PathBuf::from(format!("src/f{i}.rs")), "x".to_string(), 1));
        }
        let brief = assemble_brief("x", "", vec![], &c);
        assert!(brief.file_digests.len() <= 12);
    }

    #[test]
    fn merge_routes_decisions_and_steers() {
        let brief = ContextBrief {
            expected_writes: vec!["src/**".to_string()],
            ..Default::default()
        };
        let report = SubagentReport {
            summary: "done".to_string(),
            facts_learned: vec!["redb is fast".to_string()],
            files_touched: vec![PathBuf::from("src/lib.rs")],
            decisions_proposed: vec!["use redb".to_string()],
            open_questions: vec!["which TTL?".to_string()],
        };
        let plan = merge_report(&report, &brief);
        assert_eq!(plan.decisions_to_record.len(), 2);
        assert_eq!(plan.steers_to_queue.len(), 1);
        assert!(plan.boundary_violations.is_empty());
        assert_eq!(plan.summary_line, "done");
    }

    #[test]
    fn merge_flags_a_boundary_violation() {
        let brief = ContextBrief {
            expected_writes: vec!["src/**".to_string()],
            ..Default::default()
        };
        let report = SubagentReport {
            files_touched: vec![PathBuf::from("README.md")],
            ..Default::default()
        };
        let plan = merge_report(&report, &brief);
        assert_eq!(plan.boundary_violations.len(), 1);
    }

    #[test]
    fn empty_summary_becomes_a_placeholder() {
        let brief = ContextBrief::default();
        let report = SubagentReport::default();
        let plan = merge_report(&report, &brief);
        assert!(plan.summary_line.contains("no summary"));
    }
}
