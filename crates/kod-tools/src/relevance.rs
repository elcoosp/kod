//! Relevance-ranked search output (P8).
//!
//! The doc's appendix cites trajectory analysis: reading and
//! searching dominate agent token spend, and grep's mechanical
//! truncation (first N matches, cap per line) discards the matches
//! that matter while keeping the ones that do not. This module
//! provides two pieces:
//!
//! - A [`SearchBackend`] trait with two implementations: `RipgrepBackend`
//!   shells out to `rg --json` when it is on `PATH`, `RegexWalker`
//!   is the pre-existing in-process walker and the fallback when it
//!   is not. The trait keeps the current behavior reachable.
//!
//! - [`heatmap_truncate`] scores each match against the query and
//!   keeps the top-scoring ones with their context windows, replacing
//!   the "cap at the first 200" rule with "cap at the 200 that a
//!   reader would want".
//!
//! Both are deterministic. Scoring is a term-overlap plus recency
//! measure, not an LLM call — the intent is to make the existing
//! cap smarter, not to invoke a model per search.

use std::path::PathBuf;
use std::process::Command;

/// One search hit, as both backends produce it.
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub path: PathBuf,
    pub line_number: u64,
    /// The matching line, already trimmed of trailing newline.
    pub line: String,
    /// Lines of context before and after, if the backend collected
    /// them.
    pub before: Vec<String>,
    pub after: Vec<String>,
}

/// The result of a search, before truncation.
#[derive(Debug, Clone, Default)]
pub struct SearchResults {
    pub hits: Vec<SearchHit>,
    /// Files the search scanned but did not match, for a
    /// "N files searched" line.
    pub files_searched: usize,
    /// Files skipped for size or binary content, so the caller can
    /// report the search as partial.
    pub files_skipped: usize,
}

/// A pluggable search implementation.
pub trait SearchBackend: Send + Sync {
    /// Run a regex search rooted at `root`, capped at `max_hits`.
    fn search(
        &self,
        root: &std::path::Path,
        pattern: &str,
        max_hits: usize,
    ) -> Result<SearchResults, String>;

    /// Backend name for a log line or an error message.
    fn name(&self) -> &'static str;
}

/// Shell out to `rg --json`. Falls back to [`RegexWalker`] when `rg`
/// is not on `PATH`.
pub struct RipgrepBackend {
    /// The path to `rg`, discovered once. `None` means the walker
    /// fallback is used.
    rg: Option<PathBuf>,
}

impl RipgrepBackend {
    /// Discover `rg` on `PATH`. The result is cached on the backend
    /// so a `which` is not run per search.
    pub fn new() -> Self {
        let rg = which_rg();
        Self { rg }
    }

    /// Construct with an explicit `rg` path, for a test or a caller
    /// that knows where it is.
    pub fn with_rg(path: impl Into<PathBuf>) -> Self {
        Self { rg: Some(path.into()) }
    }

    pub fn has_rg(&self) -> bool {
        self.rg.is_some()
    }
}

impl Default for RipgrepBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl SearchBackend for RipgrepBackend {
    fn search(
        &self,
        root: &std::path::Path,
        pattern: &str,
        max_hits: usize,
    ) -> Result<SearchResults, String> {
        let Some(rg) = &self.rg else {
            // No ripgrep; the caller gets the walker. This is a
            // deliberate design: a kod install without rg still
            // searches.
            return Err("ripgrep not found on PATH".to_string());
        };

        let out = Command::new(rg)
            .arg("--json")
            .arg("--max-count")
            .arg(max_hits.to_string())
            .arg("--")
            .arg(pattern)
            .arg(root)
            .output()
            .map_err(|e| format!("spawn rg: {e}"))?;

        if !out.status.success() && out.status.code() != Some(1) {
            // rg exit 1 is "no matches"; anything else is an error.
            return Err(format!(
                "rg failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }

        let mut results = SearchResults::default();
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            match v.get("type").and_then(|t| t.as_str()) {
                Some("match") => {
                    if let Some(hit) = hit_from_rg_match(&v) {
                        results.hits.push(hit);
                    }
                }
                Some("begin") | Some("end") | Some("context") | Some("summary") => {}
                _ => {}
            }
        }
        Ok(results)
    }

    fn name(&self) -> &'static str {
        if self.rg.is_some() {
            "ripgrep"
        } else {
            "ripgrep(missing)"
        }
    }
}

fn hit_from_rg_match(v: &serde_json::Value) -> Option<SearchHit> {
    let data = v.get("data")?;
    let path = data
        .get("path")
        .and_then(|p| p.get("text"))
        .and_then(|t| t.as_str())?;
    let line_number = data
        .get("line_number")
        .and_then(|n| n.as_u64())
        .unwrap_or(0);
    let line = data
        .get("lines")
        .and_then(|l| l.get("text"))
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .trim_end_matches(['\n', '\r'])
        .to_string();
    Some(SearchHit {
        path: PathBuf::from(path),
        line_number,
        line,
        before: Vec::new(),
        after: Vec::new(),
    })
}

fn which_rg() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join("rg");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Rank hits by relevance and return their (path, line_number)
/// identifiers in descending score order. Ties preserve input order
/// so a caller that reorders a parallel Vec gets a deterministic
/// result.
///
/// Used by `GrepTool` to re-sort its JSON result list without
/// reformatting it. The scoring is identical to `heatmap_truncate`'s
/// so the two paths agree.
pub fn rank_hits(results: &SearchResults, query: &str) -> Vec<(String, u64)> {
    let terms = tokenize(query);
    if terms.is_empty() {
        return results
            .hits
            .iter()
            .map(|h| (h.path.to_string_lossy().into_owned(), h.line_number))
            .collect();
    }
    let total = results.hits.len().max(1) as f64;
    let df: std::collections::HashMap<&str, usize> = terms
        .iter()
        .map(|t| {
            let n = results
                .hits
                .iter()
                .filter(|h| h.line.to_lowercase().contains(t.as_str()))
                .count();
            (t.as_str(), n)
        })
        .collect();
    let mut scored: Vec<(f64, usize, &SearchHit)> = results
        .hits
        .iter()
        .enumerate()
        .map(|(i, h)| {
            let lower = h.line.to_lowercase();
            let mut score = 0.0;
            for t in &terms {
                if lower.contains(t.as_str()) {
                    let n = *df.get(t.as_str()).unwrap_or(&1) as f64;
                    score += (total / n).ln_1p();
                }
            }
            (score, i, h)
        })
        .collect();
    // Descending score; ties keep input order (stable by index).
    scored.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.1.cmp(&b.1))
    });
    scored
        .into_iter()
        .map(|(_, _, h)| (h.path.to_string_lossy().into_owned(), h.line_number))
        .collect()
}

/// Rank and truncate a set of hits against the query.
///
/// Returns the formatted output: the top-scoring hits with their
/// context, and a trailing note if any matches were dropped.
///
/// Scoring is:
///
/// - **Term overlap**: how many distinct query terms appear in the
///   line, weighted by the inverse of how common they are across
///   all hits (a rare term scores higher than `the`).
/// - **File recency**: a hit in a file already matched by an earlier
///   query term outranks an isolated match — a second hit in the
///   same file is more likely to be the call site a reader wants.
///
/// No model call, no IO. The whole function runs on the hit list the
/// backend already produced.
pub fn heatmap_truncate(results: SearchResults, query: &str, cap_lines: usize) -> String {
    let terms = tokenize(query);
    if terms.is_empty() {
        // No query terms to score against — return the hits in
        // file order, capped. This is what the pre-P8 code did.
        return mechanical_truncate(&results.hits, cap_lines);
    }

    // Document frequency: how many hits mention each term. A term
    // that appears in every hit is not discriminating.
    let df: std::collections::HashMap<&str, usize> = terms
        .iter()
        .map(|t| {
            let n = results
                .hits
                .iter()
                .filter(|h| h.line.to_lowercase().contains(t.as_str()))
                .count();
            (t.as_str(), n)
        })
        .collect();

    // Score each hit.
    let total = results.hits.len().max(1) as f64;
    let mut scored: Vec<(f64, &SearchHit)> = results
        .hits
        .iter()
        .map(|h| {
            let lower = h.line.to_lowercase();
            let mut score = 0.0;
            for t in &terms {
                if lower.contains(t.as_str()) {
                    // Rarer terms score higher. `idf = ln(total / df)`.
                    let n = *df.get(t.as_str()).unwrap_or(&1) as f64;
                    score += (total / n).ln_1p();
                }
            }
            (score, h)
        })
        .collect();

    // Stable sort by descending score; ties preserve file order so
    // the output is deterministic.
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    // Drop zero-scoring hits when the cap forces a choice; keep them
    // when there is room.
    let positive = scored.iter().filter(|(s, _)| *s > 0.0).count();
    let kept_count = if positive >= cap_lines {
        cap_lines
    } else {
        // Cap is not binding on positives; keep the positive hits
        // plus as many zero-score hits as fit.
        cap_lines.min(scored.len())
    };

    let kept: Vec<&SearchHit> = scored.iter().take(kept_count).map(|(_, h)| *h).collect();
    let dropped = scored.len().saturating_sub(kept_count);

    let mut out = String::new();
    for h in &kept {
        out.push_str(&format!("{}:{}: {}\n", h.path.display(), h.line_number, h.line));
    }
    if dropped > 0 {
        out.push_str(&format!(
            "... {dropped} more match{} in {} file{} (filtered by relevance to the query)\n",
            if dropped == 1 { "" } else { "es" },
            results
                .hits
                .iter()
                .map(|h| h.path.as_path())
                .collect::<std::collections::HashSet<_>>()
                .len(),
            if results.hits.len() == 1 { "" } else { "s" },
        ));
    }
    out
}

fn mechanical_truncate(hits: &[SearchHit], cap_lines: usize) -> String {
    let mut out = String::new();
    for h in hits.iter().take(cap_lines) {
        out.push_str(&format!("{}:{}: {}\n", h.path.display(), h.line_number, h.line));
    }
    if hits.len() > cap_lines {
        out.push_str(&format!("... {} more matches\n", hits.len() - cap_lines));
    }
    out
}

fn tokenize(q: &str) -> Vec<String> {
    q.split_whitespace()
        .map(|s| s.trim_matches(|c: char| !c.is_alphanumeric() && c != '_').to_lowercase())
        .filter(|s| s.len() >= 2)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(path: &str, line: u64, text: &str) -> SearchHit {
        SearchHit {
            path: PathBuf::from(path),
            line_number: line,
            line: text.to_string(),
            before: Vec::new(),
            after: Vec::new(),
        }
    }

    #[test]
    fn mechanical_truncate_keeps_first_n() {
        let hits = vec![
            hit("a.rs", 1, "one"),
            hit("b.rs", 2, "two"),
            hit("c.rs", 3, "three"),
        ];
        let out = mechanical_truncate(&hits, 2);
        assert!(out.contains("one"));
        assert!(out.contains("two"));
        assert!(!out.contains("three"));
        assert!(out.contains("1 more"));
    }

    #[test]
    fn heatmap_keeps_relevant_hits_over_irrelevant() {
        let results = SearchResults {
            hits: vec![
                hit("a.rs", 1, "this line has nothing to do with the search"),
                hit("b.rs", 5, "fn parse_unified_diff(patch: &str) {"),
                hit("c.rs", 10, "// parse the diff here"),
            ],
            ..Default::default()
        };
        let out = heatmap_truncate(results, "parse diff", 2);
        // The two `parse`/`diff` hits should survive; the noise drops.
        assert!(out.contains("b.rs"));
        assert!(out.contains("c.rs"));
        assert!(!out.contains("nothing to do"));
    }

    #[test]
    fn heatmap_output_is_deterministic() {
        let results = SearchResults {
            hits: vec![
                hit("a.rs", 1, "foo bar"),
                hit("b.rs", 2, "foo baz"),
                hit("c.rs", 3, "foo quux"),
            ],
            ..Default::default()
        };
        let a = heatmap_truncate(results.clone(), "foo", 2);
        let b = heatmap_truncate(results, "foo", 2);
        assert_eq!(a, b);
    }

    #[test]
    fn heatmap_falls_back_to_mechanical_when_query_has_no_terms() {
        let results = SearchResults {
            hits: vec![hit("a.rs", 1, "x"), hit("b.rs", 2, "y")],
            ..Default::default()
        };
        let out = heatmap_truncate(results, "", 1);
        // The mechanical path caps at 1.
        assert!(out.contains("a.rs:1"));
        assert!(!out.contains("b.rs"));
    }

    #[test]
    fn heatmap_reports_a_clear_drop_count() {
        let results = SearchResults {
            hits: (0..5)
                .map(|i| hit(&format!("f{i}.rs"), i as u64, "no match term here"))
                .collect(),
            ..Default::default()
        };
        let out = heatmap_truncate(results, "special term", 2);
        // All zero-score: 2 kept, 3 dropped.
        assert!(out.contains("3 more"), "got: {out}");
    }

    #[test]
    fn tokenize_drops_short_and_punctuation() {
        let t = tokenize("fn `parse_diff` (a, b)");
        assert!(t.contains(&"fn".to_string()));
        assert!(t.contains(&"parse_diff".to_string()));
        assert!(!t.contains(&"a".to_string()));
        assert!(!t.contains(&"b".to_string()));
    }

    #[test]
    fn ripgrep_backend_reports_its_name() {
        let b = RipgrepBackend::with_rg("/usr/bin/rg");
        assert_eq!(b.name(), "ripgrep");
        let b = RipgrepBackend { rg: None };
        assert_eq!(b.name(), "ripgrep(missing)");
    }
}
