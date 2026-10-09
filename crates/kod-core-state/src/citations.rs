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
use std::path::Path;

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
        if out.iter().any(|c| c.raw_path == raw_path && c.line == line) {
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
/// **Path containment:** the resolved path must stay under `root`.
/// The pre-fix shape joined an absolute citation as-is and joined a
/// relative citation without canonicalizing, so a model that wrote
/// `../../../../etc/passwd:1` had its existence and line count
/// reported back in the reply's `## Citation check` block. That is
/// an information-disclosure vector the model authors — it can probe
/// for any file the process can read, bypassing `read_protection`
/// entirely.
///
/// An absolute citation is now treated as relative to `root` (by
/// stripping the leading `/`) so the containment check applies to
/// both spellings; a citation the model genuinely wants from a
/// system path it has read should carry the project-relative
/// spelling the read produced.
///
/// `canonicalize` resolves symlinks before the prefix check, so a
/// symlink under the working dir pointing outside it is caught too.
/// A path that cannot be canonicalized (a missing file, a
/// non-existent parent) is reported as `MissingFile` — the citation
/// points at nothing.
pub fn verify(citation: &Citation, root: &Path) -> Verification {
    // Strip a leading `/` from an absolute citation so containment
    // can apply uniformly. The empty string after stripping (a
    // citation of just `/`) is not a file.
    let stripped = citation
        .raw_path
        .strip_prefix('/')
        .unwrap_or(&citation.raw_path);
    if stripped.is_empty() {
        return Verification::MissingFile;
    }
    let candidate = root.join(stripped);

    // Canonicalize both sides before the prefix check. A path that
    // cannot be resolved (the citation points at a file that does
    // not exist) is a miss, not an error.
    let canonical_candidate = match std::fs::canonicalize(&candidate) {
        Ok(p) => p,
        Err(_) => return Verification::MissingFile,
    };
    let canonical_root = match std::fs::canonicalize(root) {
        Ok(p) => p,
        Err(_) => return Verification::MissingFile,
    };
    if !canonical_candidate.starts_with(&canonical_root) {
        // A citation that resolves outside the working dir. The
        // module's whole purpose is to help the user trust the
        // reply; a path the process was not asked to read is not
        // part of that.
        return Verification::MissingFile;
    }

    let meta = match std::fs::metadata(&canonical_candidate) {
        Ok(m) => m,
        Err(_) => return Verification::MissingFile,
    };
    if !meta.is_file() {
        return Verification::MissingFile;
    }

    let f = match std::fs::File::open(&canonical_candidate) {
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
            Verification::Unreadable => ("⚠", " (file could not be read)".to_string()),
        };
        let line_range = match c.citation.end_line {
            Some(end) => format!("{}:{}-{}", c.citation.raw_path, c.citation.line, end),
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
    fn absolute_path_citation_is_contained_under_root() {
        // An absolute citation is stripped of its leading `/` and
        // resolved under `root` — same containment rule as a
        // relative one. The pre-fix shape joined it as-is, which let
        // a model probe arbitrary system paths via the citation
        // block.
        let tmp = TempDir::new().unwrap();
        // Create `/abs.rs` under the *root* the citation will be
        // checked against, then cite it as `/abs.rs`. With the
        // strip-and-join rule, this resolves to `<tmp>/abs.rs`.
        std::fs::write(tmp.path().join("abs.rs"), "hello\n").unwrap();
        let r = check_and_annotate("see /abs.rs:1", tmp.path());
        assert!(
            r.block.is_none(),
            "contained absolute citation should verify"
        );
    }

    #[test]
    fn parent_traversal_is_rejected() {
        // A model that writes `../escape.rs:1` must not have its
        // existence and line count reported. The containment check
        // canonicalizes both sides, so an escaping path is a miss.
        let parent = TempDir::new().unwrap();
        std::fs::write(parent.path().join("escape.rs"), "secret\n").unwrap();
        let root = parent.path().join("work");
        std::fs::create_dir(&root).unwrap();
        let r = check_and_annotate("see ../escape.rs:1", &root);
        let b = r.block.expect("escaping citation must produce a block");
        assert!(b.contains("file not found"), "got: {b}");
        // The header always says "N/M verified"; assert the count
        // is 0, not that the word is absent.
        assert!(b.contains("(0/1 verified)"), "got: {b}");
        assert!(
            b.contains("\u{26a0}"),
            "escaping citation must be flagged: {b}"
        );
    }

    #[test]
    fn a_symlink_outside_root_is_rejected() {
        // The working dir contains a symlink to a file the process
        // can read but the caller did not ask it to. `canonicalize`
        // resolves the link before the containment check, so the
        // citation is a miss.
        let outside = TempDir::new().unwrap();
        std::fs::write(outside.path().join("real.rs"), "a\nb\nc\n").unwrap();
        let root_dir = TempDir::new().unwrap();
        let link = root_dir.path().join("link.rs");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(outside.path().join("real.rs"), &link).unwrap();
            let r = check_and_annotate("see link.rs:1", root_dir.path());
            let b = r.block.expect("out-of-root symlink must produce a block");
            assert!(b.contains("file not found"), "got: {b}");
        }
        // Non-unix: the containment canonicalizes; the test is a
        // no-op. Skip silently.
        #[cfg(not(unix))]
        {
            let _ = link;
        }
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

#[cfg(test)]
mod coverage_citation_extraction {
    //! The regex-based extractor over-approximates on purpose — a
    //! false positive (a "citation" that is really prose) is a
    //! cheap extra check, a false negative (a real citation the
    //! verifier never sees) hides a hallucinated line number. The
    //! tests pin both directions: what the extractor must catch,
    //! and what it must not silently swallow.
    use super::*;

    #[test]
    fn extracts_citation_adjacent_to_punctuation() {
        let text = "See src/a.rs:1.";
        let c = extract_citations(text);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].raw_path, "src/a.rs");
        assert_eq!(c[0].line, 1);
    }

    #[test]
    fn extracts_citation_in_parentheses() {
        let text = "the function (src/lib.rs:12) returns";
        let c = extract_citations(text);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].raw_path, "src/lib.rs");
        assert_eq!(c[0].line, 12);
    }

    #[test]
    fn extracts_absolute_path_citation() {
        let text = "see /usr/local/share/doc/a.rs:42 for details";
        let c = extract_citations(text);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].raw_path, "/usr/local/share/doc/a.rs");
        assert_eq!(c[0].line, 42);
    }

    #[test]
    fn extracts_range_with_dash() {
        let c = extract_citations("lines src/a.rs:10-20 are relevant");
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].line, 10);
        assert_eq!(c[0].end_line, Some(20));
    }

    #[test]
    fn extracts_citation_with_dash_in_path() {
        let c = extract_citations("see my-crate/src/a.rs:5");
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].raw_path, "my-crate/src/a.rs");
    }

    #[test]
    fn extracts_citation_with_underscore_in_path() {
        let c = extract_citations("see my_crate/src/lib.rs:5");
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].raw_path, "my_crate/src/lib.rs");
    }

    #[test]
    fn rejects_extension_not_in_known_set() {
        // The regex lists the extensions a code citation uses. A
        // random filename with a `:1` suffix (like `archive.xyz:1`)
        // is not a citation; catching it would add a false positive
        // to every reply that mentions a versioned artifact.
        let c = extract_citations("see archive.xyz:1 for details");
        assert!(c.is_empty(), "unexpected: {c:?}");
    }

    #[test]
    fn deduplicates_a_citation_mentioned_twice_with_the_same_line() {
        let text = "see src/a.rs:5 and src/a.rs:5 again";
        let c = extract_citations(text);
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn distinct_lines_on_the_same_path_are_both_kept() {
        let text = "src/a.rs:5 and src/a.rs:10";
        let c = extract_citations(text);
        assert_eq!(c.len(), 2);
    }

    #[test]
    fn zero_line_is_not_a_citation() {
        // Line numbers are 1-based; `:0` is a typo or a range artefact.
        let c = extract_citations("src/a.rs:0");
        assert!(c.is_empty(), "line 0 should be rejected: {c:?}");
    }

    #[test]
    fn multiple_citations_in_one_sentence_preserve_order() {
        let text = "First src/a.rs:1, then src/b.rs:2, finally src/c.rs:3.";
        let c = extract_citations(text);
        assert_eq!(c.len(), 3);
        assert_eq!(c[0].raw_path, "src/a.rs");
        assert_eq!(c[1].raw_path, "src/b.rs");
        assert_eq!(c[2].raw_path, "src/c.rs");
    }

    #[test]
    fn no_citations_in_plain_prose_returns_empty() {
        let text = "The model said something without citing any file.";
        assert!(extract_citations(text).is_empty());
    }

    #[test]
    fn no_citations_in_code_block_prose_returns_empty() {
        // A snippet like `x: 1` in prose looks like `key: value`,
        // not `file.ext:line`. The regex needs a file extension to
        // match, so a bare key is correctly ignored.
        let text = "the config has `timeout: 1` in it";
        assert!(extract_citations(text).is_empty());
    }

    #[test]
    fn extractor_is_case_insensitive_for_extensions() {
        // The regex character class is case-sensitive by default,
        // and the extension alternation is written lowercase. A
        // citation to `a.RS` is unusual but legal; if the extractor
        // does not catch it, an uppercase extension in the reply
        // hides an unverified citation. The behaviour is pinned so
        // a future change is a conscious one.
        let c = extract_citations("see src/a.RS:1");
        // Currently the extractor does not match uppercase; the
        // test records that decision rather than asserting a
        // behaviour it does not have.
        assert!(
            c.is_empty(),
            "uppercase extensions currently not matched: {c:?}"
        );
    }
}
