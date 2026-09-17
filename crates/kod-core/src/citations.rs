//! Citation verification for Research-mode replies (D6.6 M9).
//!
//! A research answer that says "the bug is in `parser.rs:42`" is
//! only useful if line 42 actually contains something. Models
//! hallucinate line numbers; the cheapest fix is a deterministic
//! post-processing pass that reads the cited location and reports
//! what it found.
//!
//! # Scope
//!
//! Two checks, both purely local (no LLM call, no network):
//!
//! 1. The cited path resolves and is a file.
//! 2. The cited line number is within the file's line count.
//!
//! Both must hold for a citation to be "verified". A citation that
//! fails either check is annotated with `⚠` and the reason.
//!
//! # What is deliberately not checked
//!
//! A "does the cited line contain a token from the surrounding
//! prose" heuristic sounds attractive and is not implemented. A
//! wrong "unverified" annotation on a real citation is worse than no
//! annotation at all — a user who sees ⚠ on a correct citation
//! learns to ignore ⚠. The pass reports only what it can prove: the
//! location exists, or it does not.
//!
//! # When the block appears
//!
//! `check_and_annotate` returns `block: None` when either (a) there
//! are no citations in the reply, or (b) every citation verified.
//! A clean reply stays clean.

use regex::Regex;
use std::path::{Path, PathBuf};

/// Largest line number we will scan to. A file with more lines than
/// this is either generated or is not the kind of file a citation
/// points into; either way, scanning past a million lines buys
/// nothing and costs wall time.
const MAX_SCAN_LINES: u32 = 1_000_000;

/// One parsed `file.ext:line` (or `file.ext:line-end`) citation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Citation {
    /// Path as it appeared in the reply, with its extension — e.g.
    /// `src/parser.rs`. Not resolved yet.
    pub raw_path: String,
    /// 1-based line number.
    pub line: u32,
    /// When the citation names a range (`parser.rs:42-45`), the
    /// second number. `None` for a single-line citation.
    pub end_line: Option<u32>,
}

/// Outcome of checking one citation against the filesystem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verification {
    /// The file exists and the cited line is within its line count.
    Verified,
    /// The path did not resolve to a file (missing, a directory, or
    /// a permission error at the `metadata` call).
    MissingFile,
    /// The file exists but is shorter than the cited line. `max` is
    /// the file's actual line count.
    LineOutOfRange { max: u32 },
    /// The file exists and is a regular file, but opening it for
    /// the line count failed. Rare; reported rather than treated as
    /// a silent success.
    Unreadable,
}

/// A citation paired with its verification.
#[derive(Debug, Clone)]
pub struct CitationCheck {
    pub citation: Citation,
    pub verification: Verification,
}

/// Result of running the check on a reply.
#[derive(Debug, Clone, Default)]
pub struct AnnotatedReply {
    /// The reply text, with the block appended when one was rendered.
    pub text: String,
    /// The block that was appended, if any. Provided separately so
    /// the streaming path can emit it as a chunk without parsing it
    /// back out of `text`.
    pub block: Option<String>,
}

/// The one compiled citation regex, built once.
///
/// The path prefix character class includes `.`, `/`, and `-` so
/// `./foo.rs`, `../foo.rs`, and `my-dir/foo.rs` all match. The
/// greedy prefix walks back to the last dot that allows a valid
/// extension + `:` + digits to follow.
fn citation_regex() -> &'static Regex {
    use std::sync::OnceLock;
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"([A-Za-z0-9_./\-]+)\.(toml|yaml|bash|json|java|hpp|cxx|yml|tsx|jsx|cpp|ts|js|rs|py|go|rb|md|sh|cc|c|h):(\d+)(?:-(\d+))?",
        )
        .expect("citation regex compiles")
    })
}

/// Every distinct citation in `text`, in the order they first appear.
///
/// Duplicates (same path, same line) collapse to one entry: a reply
/// that references the same line twice does not need two checks.
pub fn extract_citations(text: &str) -> Vec<Citation> {
    let re = citation_regex();
    let mut out: Vec<Citation> = Vec::new();
    for cap in re.captures_iter(text) {
        let path = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        let ext = cap.get(2).map(|m| m.as_str()).unwrap_or("");
        let line: u32 = match cap.get(3).and_then(|m| m.as_str().parse().ok()) {
            Some(n) if n > 0 => n,
            _ => continue,
        };
        let end_line: Option<u32> = cap.get(4).and_then(|m| m.as_str().parse().ok());
        if path.is_empty() || ext.is_empty() {
            continue;
        }
        let raw_path = format!("{path}.{ext}");
        if out
            .iter()
            .any(|c| c.raw_path == raw_path && c.line == line)
        {
            continue;
        }
        out.push(Citation {
            raw_path,
            line,
            end_line,
        });
    }
    out
}

/// Verify one citation against `root`.
///
/// A relative path is resolved against `root`; an absolute path is
/// used as-is. Both are legal — a reply may cite the project
/// (`src/lib.rs:1`) or a system file (`/etc/hosts:1`).
pub fn verify(citation: &Citation, root: &Path) -> Verification {
    let candidate: PathBuf = if Path::new(&citation.raw_path).is_absolute() {
        PathBuf::from(&citation.raw_path)
    } else {
        root.join(&citation.raw_path)
    };

    let meta = match std::fs::metadata(&candidate) {
        Ok(m) => m,
        Err(_) => return Verification::MissingFile,
    };
    if !meta.is_file() {
        return Verification::MissingFile;
    }

    let f = match std::fs::File::open(&candidate) {
        Ok(f) => f,
        Err(_) => return Verification::Unreadable,
    };

    // Count lines up to the cited one. `take(n).count()` returns
    // `min(file_lines, n)`, which is exactly the two outcomes we
    // need: equal to `line` means the line exists; less than `line`
    // means the file is shorter and `count` is its true length.
    use std::io::BufRead;
    let take = citation.line.min(MAX_SCAN_LINES) as usize;
    let count = std::io::BufReader::new(f).lines().take(take).count() as u32;

    if count < citation.line {
        Verification::LineOutOfRange { max: count }
    } else {
        Verification::Verified
    }
}

/// Render the `## Citation check (N/M verified)` block for a list of
/// checks. Callers pass at least one non-verified check; the block
/// is not produced for an all-clean reply.
pub fn render_block(checks: &[CitationCheck]) -> String {
    let total = checks.len();
    let ok = checks
        .iter()
        .filter(|c| c.verification == Verification::Verified)
        .count();
    let mut out = format!("## Citation check ({ok}/{total} verified)\n\n");
    for c in checks {
        let (marker, note) = match &c.verification {
            Verification::Verified => ("✓", String::new()),
            Verification::MissingFile => ("⚠", " (file not found)".to_string()),
            Verification::LineOutOfRange { max } => (
                "⚠",
                format!(
                    " (line {} is out of range; the file has {max} lines)",
                    c.citation.line
                ),
            ),
            Verification::Unreadable => {
                ("⚠", " (file could not be read)".to_string())
            }
        };
        let line_range = match c.citation.end_line {
            Some(end) => format!(
                "{}:{}-{}",
                c.citation.raw_path, c.citation.line, end
            ),
            None => format!("{}:{}", c.citation.raw_path, c.citation.line),
        };
        out.push_str(&format!("{marker} {line_range}{note}\n"));
    }
    out
}

/// The whole pass, in one call: extract citations from `text`, verify
/// each against `root`, and return the reply with the block appended
/// when there is anything to report.
///
/// A reply with no citations, or with every citation verified, is
/// returned unchanged (`block: None`).
pub fn check_and_annotate(text: &str, root: &Path) -> AnnotatedReply {
    let citations = extract_citations(text);
    if citations.is_empty() {
        return AnnotatedReply {
            text: text.to_string(),
            block: None,
        };
    }
    let checks: Vec<CitationCheck> = citations
        .iter()
        .map(|c| CitationCheck {
            citation: c.clone(),
            verification: verify(c, root),
        })
        .collect();
    let all_ok = checks
        .iter()
        .all(|c| c.verification == Verification::Verified);
    if all_ok {
        return AnnotatedReply {
            text: text.to_string(),
            block: None,
        };
    }
    let block = render_block(&checks);
    let text = format!("{text}\n\n{block}");
    AnnotatedReply {
        text,
        block: Some(block),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn extracts_single_citation_from_prose() {
        let text = "The bug is in src/parser.rs:42, I think.";
        let cites = extract_citations(text);
        assert_eq!(cites.len(), 1);
        assert_eq!(cites[0].raw_path, "src/parser.rs");
        assert_eq!(cites[0].line, 42);
        assert!(cites[0].end_line.is_none());
    }

    #[test]
    fn extracts_range_citation() {
        let text = "See src/parser.rs:42-45 for context.";
        let cites = extract_citations(text);
        assert_eq!(cites.len(), 1);
        assert_eq!(cites[0].line, 42);
        assert_eq!(cites[0].end_line, Some(45));
    }

    #[test]
    fn extracts_multiple_citations_in_order() {
        let text = "First src/a.rs:1 then src/b.rs:2 and src/c.rs:3.";
        let cites = extract_citations(text);
        let paths: Vec<&str> = cites.iter().map(|c| c.raw_path.as_str()).collect();
        assert_eq!(paths, vec!["src/a.rs", "src/b.rs", "src/c.rs"]);
    }

    #[test]
    fn deduplicates_identical_citations() {
        let text = "src/a.rs:1 ... src/a.rs:1";
        let cites = extract_citations(text);
        assert_eq!(cites.len(), 1);
    }

    #[test]
    fn does_not_match_urls_or_unqualified_numbers() {
        assert!(extract_citations("see http://example.com:8080/x").is_empty());
        assert!(extract_citations("line 42 alone").is_empty());
        assert!(extract_citations("config.foo:1").is_empty());
    }

    #[test]
    fn no_citations_produces_no_block() {
        let tmp = TempDir::new().unwrap();
        let r = check_and_annotate("No citations here.", tmp.path());
        assert!(r.block.is_none());
        assert_eq!(r.text, "No citations here.");
    }

    #[test]
    fn verified_citation_produces_no_block() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "line 1\nline 2\nline 3\n").unwrap();
        let r = check_and_annotate("see a.rs:2 for details", tmp.path());
        assert!(
            r.block.is_none(),
            "verified citation should not produce a block: {:?}",
            r.block
        );
    }

    #[test]
    fn missing_file_produces_block() {
        let tmp = TempDir::new().unwrap();
        let r = check_and_annotate("see nope.rs:1 for details", tmp.path());
        assert!(r.block.is_some());
        let b = r.block.as_ref().unwrap();
        assert!(b.contains("0/1 verified"), "got: {b}");
        assert!(b.contains("⚠"), "got: {b}");
        assert!(b.contains("nope.rs:1"), "got: {b}");
        assert!(b.contains("file not found"), "got: {b}");
    }

    #[test]
    fn line_out_of_range_produces_specific_note() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("short.rs"), "one\ntwo\n").unwrap();
        let r = check_and_annotate("see short.rs:42", tmp.path());
        let b = r.block.expect("block");
        assert!(b.contains("out of range"), "got: {b}");
        assert!(b.contains("has 2 lines"), "got: {b}");
    }

    #[test]
    fn mixed_results_report_the_counts() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("real.rs"), "a\nb\nc\n").unwrap();
        let text = "real.rs:1 and ghost.rs:1 and real.rs:99";
        let r = check_and_annotate(text, tmp.path());
        let b = r.block.expect("block");
        // One verified (real.rs:1), two failures.
        assert!(b.contains("1/3 verified"), "got: {b}");
        assert!(b.contains("✓ real.rs:1"), "got: {b}");
        assert!(b.contains("⚠ ghost.rs:1"), "got: {b}");
        assert!(b.contains("⚠ real.rs:99"), "got: {b}");
    }

    #[test]
    fn absolute_path_citation_works() {
        let tmp = TempDir::new().unwrap();
        let f = tmp.path().join("abs.rs");
        std::fs::write(&f, "hello\n").unwrap();
        let abs = f.to_string_lossy().to_string();
        let text = format!("see {abs}:1");
        let r = check_and_annotate(&text, Path::new("/"));
        assert!(r.block.is_none(), "absolute citation should verify");
    }

    #[test]
    fn directory_path_is_reported_as_missing() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join("dir.rs")).unwrap();
        let r = check_and_annotate("see dir.rs:1", tmp.path());
        let b = r.block.expect("block");
        assert!(b.contains("file not found"), "got: {b}");
    }

    #[test]
    fn render_block_lists_every_check() {
        let checks = vec![
            CitationCheck {
                citation: Citation {
                    raw_path: "a.rs".into(),
                    line: 1,
                    end_line: None,
                },
                verification: Verification::Verified,
            },
            CitationCheck {
                citation: Citation {
                    raw_path: "b.rs".into(),
                    line: 10,
                    end_line: None,
                },
                verification: Verification::LineOutOfRange { max: 5 },
            },
        ];
        let block = render_block(&checks);
        assert!(block.starts_with("## Citation check (1/2 verified)"));
        assert!(block.contains("✓ a.rs:1"));
        assert!(block.contains("⚠ b.rs:10"));
    }

    #[test]
    fn annotated_reply_appends_block_after_blank_line() {
        let tmp = TempDir::new().unwrap();
        let r = check_and_annotate("see nope.rs:1", tmp.path());
        assert!(
            r.text.starts_with("see nope.rs:1\n\n"),
            "block must be separated by a blank line: {:?}",
            r.text
        );
    }
}
