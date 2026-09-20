//! Unified-diff patching for `patch_file`.
//!
//! `write_file` overwrites. A 2000-line file needs 2000 lines of prompt
//! to rewrite; a 10-line patch needs 30. On local models with an 8k
//! context window that is the difference between "can edit this file"
//! and "cannot". A diff is also a reviewable artifact, which is what
//! makes `--dry-run` natural: run the agent, show every pending diff,
//! let the user approve before anything touches disk.
//!
//! Uses the `similar` crate's unified-diff format. The parser is
//! deliberately strict: a patch that does not apply cleanly is rejected
//! with the context that failed to match, so the model sees its own
//! mistake instead of silently truncating a file.

use kod_error::{KodError, Result};
use similar::TextDiff;

/// One hunk in a parsed patch.
#[derive(Debug, Clone)]
pub struct Hunk {
    pub old_start: usize,
    pub old_lines: usize,
    pub new_start: usize,
    pub new_lines: usize,
    pub lines: Vec<HunkLine>,
}

#[derive(Debug, Clone)]
pub enum HunkLine {
    Context(String),
    Remove(String),
    Add(String),
}

/// Apply a unified diff to `original`, returning the patched text.
///
/// Fails with a descriptive error when a hunk's context does not match
/// — that is the failure mode a model must see to correct itself.
pub fn apply_unified_diff(original: &str, patch: &str) -> Result<String> {
    let hunks = parse_unified_diff(patch)?;
    if hunks.is_empty() {
        return Err(KodError::InvalidParameters {
            reason: "patch contains no hunks".to_string(),
        });
    }
    let mut lines: Vec<String> = original.split('\n').map(|s| s.to_string()).collect();
    let had_trailing_newline = original.ends_with('\n');
    if had_trailing_newline && lines.last().map(|s| s.is_empty()).unwrap_or(false) {
        lines.pop();
    }

    let mut offset: isize = 0;
    for hunk in &hunks {
        let target = (hunk.old_start as isize - 1 + offset) as usize;
        if target > lines.len() {
            return Err(KodError::InvalidParameters {
                reason: format!(
                    "hunk at line {} is past end of file ({} lines)",
                    hunk.old_start,
                    lines.len()
                ),
            });
        }
        let mut cursor = target;
        let mut new_lines: Vec<String> = Vec::new();
        for hl in &hunk.lines {
            match hl {
                HunkLine::Context(text) => match lines.get(cursor) {
                    Some(actual) if actual == text => {
                        new_lines.push(actual.clone());
                        cursor += 1;
                    }
                    Some(actual) => {
                        return Err(KodError::InvalidParameters {
                            reason: format!(
                                "context mismatch at line {}:\n  expected: {}\n  actual:   {}",
                                cursor + 1,
                                text,
                                actual
                            ),
                        });
                    }
                    None => {
                        return Err(KodError::InvalidParameters {
                            reason: format!(
                                "context expected at line {} but file ended",
                                cursor + 1
                            ),
                        });
                    }
                },
                HunkLine::Remove(text) => match lines.get(cursor) {
                    Some(actual) if actual == text => {
                        cursor += 1;
                    }
                    Some(actual) => {
                        return Err(KodError::InvalidParameters {
                            reason: format!(
                                "removal mismatch at line {}:\n  expected: {}\n  actual:   {}",
                                cursor + 1,
                                text,
                                actual
                            ),
                        });
                    }
                    None => {
                        return Err(KodError::InvalidParameters {
                            reason: format!(
                                "removal expected at line {} but file ended",
                                cursor + 1
                            ),
                        });
                    }
                },
                HunkLine::Add(text) => {
                    new_lines.push(text.clone());
                }
            }
        }
        let old_len = hunk.old_lines;
        let consumed = cursor - target;
        if consumed != old_len && old_len > 0 {
            return Err(KodError::InvalidParameters {
                reason: format!(
                    "hunk at line {} consumed {} source lines but header declared {}",
                    hunk.old_start, consumed, old_len
                ),
            });
        }
        lines.splice(target..cursor, new_lines);
        offset += hunk.new_lines as isize - hunk.old_lines as isize;
    }

    let mut out = lines.join("\n");
    if had_trailing_newline {
        out.push('\n');
    }
    Ok(out)
}

/// Parse a unified diff (`--- a/…`, `+++ b/…`, `@@ -l,n +l,n @@`).
pub fn parse_unified_diff(patch: &str) -> Result<Vec<Hunk>> {
    let mut hunks = Vec::new();
    let mut current: Option<Hunk> = None;
    for line in patch.lines() {
        if line.starts_with("--- ") || line.starts_with("+++ ") {
            continue;
        }
        if line.starts_with("@@") {
            if let Some(h) = current.take() {
                hunks.push(h);
            }
            current = Some(parse_hunk_header(line)?);
            continue;
        }
        let Some(h) = current.as_mut() else {
            continue;
        };
        if line.is_empty() {
            h.lines.push(HunkLine::Context(String::new()));
            continue;
        }
        let (tag, text) = line.split_at(1);
        let text = text.to_string();
        match tag {
            " " => h.lines.push(HunkLine::Context(text)),
            "-" => h.lines.push(HunkLine::Remove(text)),
            "+" => h.lines.push(HunkLine::Add(text)),
            "\\" => {}
            _ => {
                return Err(KodError::InvalidParameters {
                    reason: format!("unrecognized diff line: {:?}", line),
                });
            }
        }
    }
    if let Some(h) = current.take() {
        hunks.push(h);
    }
    Ok(hunks)
}

fn parse_hunk_header(line: &str) -> Result<Hunk> {
    let inner = line.trim_start_matches("@@").trim_end_matches("@@").trim();
    let mut parts = inner.split_whitespace();
    let old = parts
        .next()
        .ok_or_else(|| KodError::InvalidParameters {
            reason: format!("bad hunk header: {line}"),
        })?
        .trim_start_matches('-');
    let new = parts
        .next()
        .ok_or_else(|| KodError::InvalidParameters {
            reason: format!("bad hunk header: {line}"),
        })?
        .trim_start_matches('+');
    let (old_start, old_lines) = parse_range(old)?;
    let (new_start, new_lines) = parse_range(new)?;
    Ok(Hunk {
        old_start,
        old_lines,
        new_start,
        new_lines,
        lines: Vec::new(),
    })
}

fn parse_range(s: &str) -> Result<(usize, usize)> {
    match s.split_once(',') {
        Some((start, count)) => Ok((
            start.parse().map_err(|_| KodError::InvalidParameters {
                reason: format!("bad range start: {start}"),
            })?,
            count.parse().map_err(|_| KodError::InvalidParameters {
                reason: format!("bad range count: {count}"),
            })?,
        )),
        None => Ok((
            s.parse().map_err(|_| KodError::InvalidParameters {
                reason: format!("bad range: {s}"),
            })?,
            1,
        )),
    }
}

/// Render a unified diff between `old` and `new`.
///
/// Delegates to the `similar` crate's `unified_diff()` builder, which
/// emits correct `@@ -l,n +l,n @@` hunk headers. The previous hand-
/// rolled implementation wrote `@@ ... @@` group separators without
/// real hunk headers, so `apply_unified_diff` (which parses those
/// headers to locate hunks) saw zero hunks and rejected every patch
/// this function produced. `render_then_apply` is the regression test.
pub fn render_unified_diff(old: &str, new: &str, path: &str) -> String {
    let diff = TextDiff::from_lines(old, new);
    diff.unified_diff()
        .context_radius(3)
        .header(&format!("a/{path}"), &format!("b/{path}"))
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_simple_replacement() {
        let original = "line one\nline two\nline three\n";
        let patch =
            "--- a/f\n+++ b/f\n@@ -1,3 +1,3 @@\n line one\n-line two\n+LINE TWO\n line three\n";
        let result = apply_unified_diff(original, patch).unwrap();
        assert_eq!(result, "line one\nLINE TWO\nline three\n");
    }

    #[test]
    fn apply_addition() {
        let original = "a\nb\n";
        let patch = "@@ -1,2 +1,3 @@\n a\n b\n+c\n";
        let result = apply_unified_diff(original, patch).unwrap();
        assert_eq!(result, "a\nb\nc\n");
    }

    #[test]
    fn apply_removal() {
        let original = "a\nb\nc\n";
        let patch = "@@ -1,3 +1,2 @@\n a\n-b\n c\n";
        let result = apply_unified_diff(original, patch).unwrap();
        assert_eq!(result, "a\nc\n");
    }

    #[test]
    fn context_mismatch_is_a_descriptive_error() {
        let original = "a\nb\nc\n";
        let patch = "@@ -1,3 +1,3 @@\n a\n-WRONG\n+x\n c\n";
        let err = apply_unified_diff(original, patch).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("mismatch"), "got: {msg}");
        assert!(msg.contains("WRONG"), "got: {msg}");
    }

    #[test]
    fn round_trip_render_then_apply() {
        let old = "fn main() {\n    println!(\"hi\");\n}\n";
        let new = "fn main() {\n    println!(\"hello\");\n    println!(\"world\");\n}\n";
        let patch = render_unified_diff(old, new, "main.rs");
        let applied = apply_unified_diff(old, &patch).unwrap();
        assert_eq!(applied, new);
    }

    #[test]
    fn empty_patch_is_rejected() {
        let err = apply_unified_diff("a\n", "").unwrap_err();
        assert!(err.to_string().contains("no hunks"));
    }
}

#[cfg(test)]
mod coverage_diff_edge_cases {
    //! The unified-diff parser and renderer are `patch_file`'s only
    //! moving parts. A regression here corrupts files: a mis-parsed
    //! hunk header can splice content into the wrong region, an
    //! off-by-one in the header count silently drops a line. The
    //! existing tests cover the simple cases; this module pins the
    //! cases the model will hit when it edits real code.
    use super::*;

    #[test]
    fn multi_hunk_patch_applies_in_order() {
        let original = "a\nb\nc\nd\ne\nf\n";
        let patch = concat!(
            "--- a/f\n+++ b/f\n",
            "@@ -1,2 +1,2 @@\n a\n-b\n+B\n",
            "@@ -5,2 +5,2 @@\n e\n-f\n+F\n",
        );
        let out = apply_unified_diff(original, patch).unwrap();
        assert_eq!(out, "a\nB\nc\nd\ne\nF\n");
    }

    #[test]
    fn add_at_end_of_file() {
        let original = "only\n";
        let patch = "@@ -1 +1,2 @@\n only\n+more\n";
        let out = apply_unified_diff(original, patch).unwrap();
        assert_eq!(out, "only\nmore\n");
    }

    #[test]
    fn remove_from_start_of_file() {
        let original = "first\nsecond\n";
        let patch = "@@ -1,2 +1 @@\n-first\n second\n";
        let out = apply_unified_diff(original, patch).unwrap();
        assert_eq!(out, "second\n");
    }

    #[test]
    fn round_trip_no_trailing_newline() {
        let old = "a\nb";
        let new = "a\nc";
        let patch = render_unified_diff(old, new, "f");
        let applied = apply_unified_diff(old, &patch).unwrap();
        assert_eq!(applied, new);
    }

    #[test]
    fn round_trip_multiline_replacement() {
        let old = "keep\nold one\nold two\nkeep again\n";
        let new = "keep\nnew\nkeep again\n";
        let patch = render_unified_diff(old, new, "f");
        let applied = apply_unified_diff(old, &patch).unwrap();
        assert_eq!(applied, new);
    }

    #[test]
    fn round_trip_identical_inputs_returns_no_hunks() {
        let s = "same\nlines\n";
        let patch = render_unified_diff(s, s, "f");
        let hunks = parse_unified_diff(&patch).unwrap();
        assert!(hunks.is_empty(), "identical diff should have no hunks");
    }

    #[test]
    fn hunk_header_without_count_defaults_to_one() {
        // `@@ -2 +2 @@` (no comma) is a 1-line hunk by spec. A
        // parser that reads the missing count as "0 lines" would
        // skip the hunk and produce no change.
        let original = "a\nb\n";
        let patch = "@@ -2 +2 @@\n-b\n+B\n";
        let out = apply_unified_diff(original, patch).unwrap();
        assert_eq!(out, "a\nB\n");
    }

    #[test]
    fn unknown_diff_line_tag_is_rejected() {
        // A line starting with `?` is not a valid unified-diff tag.
        // The parser must say so rather than drop the line silently
        // — a dropped line in the middle of a hunk produces a
        // syntactically-valid patch that writes the wrong content.
        let patch = "@@ -1 +1 @@\n?nope\n";
        let err = parse_unified_diff(patch).unwrap_err();
        assert!(err.to_string().contains("unrecognized"), "got: {err}");
    }

    #[test]
    fn round_trip_preserves_trailing_whitespace_on_context_lines() {
        // A context line with trailing spaces must round-trip exactly.
        // The empty-line shortcut in the parser pushes `""` as
        // Context; a line of only spaces must not collapse into it.
        let old = "head\n   \ntail\n";
        let new = "head\n   \nCHANGED\n";
        let patch = render_unified_diff(old, new, "f");
        let applied = apply_unified_diff(old, &patch).unwrap();
        assert_eq!(applied, new);
    }
}

#[cfg(test)]
mod coverage_hunk_parsing {
    //! `parse_unified_diff` reads the header form the model emits
    //! on every `patch_file` call. `similar`'s `unified_diff`
    //! always emits the `-l,n +l,n` spelling, but a hand-written
    //! or model-authored patch may use the abbreviated `-l +l`
    //! form; the parser must handle both without splicing content
    //! into the wrong region.
    use super::*;

    #[test]
    fn header_without_count_defaults_to_one_line() {
        let hunks = parse_unified_diff("@@ -5 +10 @@\n-a\n+b\n").unwrap();
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].old_start, 5);
        assert_eq!(hunks[0].old_lines, 1);
        assert_eq!(hunks[0].new_start, 10);
        assert_eq!(hunks[0].new_lines, 1);
        assert_eq!(hunks[0].lines.len(), 2);
    }

    #[test]
    fn header_with_explicit_counts_uses_them() {
        let hunks = parse_unified_diff("@@ -1,3 +2,4 @@\n a\n-b\n+c\n+d\n e\n").unwrap();
        assert_eq!(hunks[0].old_start, 1);
        assert_eq!(hunks[0].old_lines, 3);
        assert_eq!(hunks[0].new_start, 2);
        assert_eq!(hunks[0].new_lines, 4);
    }

    #[test]
    fn hunk_line_tags_map_to_the_right_variants() {
        let hunks = parse_unified_diff("@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n").unwrap();
        let h = &hunks[0];
        assert_eq!(h.lines.len(), 4);
        assert!(matches!(&h.lines[0], HunkLine::Context(s) if s == "a"));
        assert!(matches!(&h.lines[1], HunkLine::Remove(s) if s == "b"));
        assert!(matches!(&h.lines[2], HunkLine::Add(s) if s == "B"));
        assert!(matches!(&h.lines[3], HunkLine::Context(s) if s == "c"));
    }

    #[test]
    fn an_empty_context_line_is_captured_as_an_empty_context() {
        // A blank line inside a hunk is a valid context line (the
        // source's own blank line). The parser's empty-line branch
        // must produce `Context("")`, not drop the line — a drop
        // shifts every subsequent line number.
        let hunks = parse_unified_diff("@@ -1,3 +1,3 @@\n a\n\n b\n").unwrap();
        let h = &hunks[0];
        assert_eq!(h.lines.len(), 3);
        assert!(matches!(&h.lines[1], HunkLine::Context(s) if s.is_empty()));
    }

    #[test]
    fn file_headers_before_the_hunks_are_ignored() {
        let patch = concat!(
            "--- a/f.rs\n",
            "+++ b/f.rs\n",
            "@@ -1,2 +1,2 @@\n a\n-b\n+c\n",
        );
        let hunks = parse_unified_diff(patch).unwrap();
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].old_start, 1);
    }

    #[test]
    fn multiple_hunks_are_returned_in_order() {
        let patch = concat!(
            "@@ -1,2 +1,2 @@\n a\n-b\n+B\n",
            "@@ -5,2 +5,2 @@\n e\n-f\n+F\n",
        );
        let hunks = parse_unified_diff(patch).unwrap();
        assert_eq!(hunks.len(), 2);
        assert_eq!(hunks[0].old_start, 1);
        assert_eq!(hunks[1].old_start, 5);
    }

    #[test]
    fn a_malformed_hunk_header_is_rejected() {
        // `@@ @@` has no ranges; the parser must reject rather
        // than guess.
        let err = parse_unified_diff("@@ @@\n a\n").unwrap_err();
        assert!(err.to_string().contains("bad hunk header"), "got: {err}");
    }

    #[test]
    fn a_non_numeric_range_is_rejected() {
        let err = parse_unified_diff("@@ -x +y @@\n a\n").unwrap_err();
        assert!(err.to_string().contains("range"), "got: {err}");
    }

    #[test]
    fn a_backslash_line_is_ignored() {
        // The `\ No newline at end of file` marker is a comment
        // for the human reader; the parser must ignore it, not
        // treat it as an unrecognized tag.
        let patch = "@@ -1 +1 @@\n a\n\\ No newline at end of file\n";
        let hunks = parse_unified_diff(patch).unwrap();
        assert_eq!(hunks[0].lines.len(), 1);
    }
}

#[cfg(test)]
mod coverage_render_unified {
    //! `render_unified_diff` produces the patch body that
    //! `apply_unified_diff` consumes — the round-trip is the
    //! contract. These pin the shapes the model sees and the
    //! paths the diff header carries, so a change to the
    //! rendering is visible as a test failure rather than a
    //! silently different diff in a tool result.
    use super::*;

    #[test]
    fn header_carries_the_path_in_the_a_b_form() {
        let d = render_unified_diff("old\n", "new\n", "src/main.rs");
        assert!(d.contains("--- a/src/main.rs"), "missing a/ header: {d}");
        assert!(d.contains("+++ b/src/main.rs"), "missing b/ header: {d}");
    }

    #[test]
    fn identical_inputs_produce_no_hunks() {
        let d = render_unified_diff("same\n", "same\n", "f.rs");
        let hunks = parse_unified_diff(&d).unwrap();
        assert!(hunks.is_empty(), "unexpected hunks: {d}");
    }

    #[test]
    fn added_line_is_marked_with_a_plus() {
        let d = render_unified_diff("a\n", "a\nb\n", "f.rs");
        assert!(d.contains("+b"), "addition not marked: {d}");
    }

    #[test]
    fn removed_line_is_marked_with_a_minus() {
        let d = render_unified_diff("a\nb\n", "a\n", "f.rs");
        assert!(d.contains("-b"), "removal not marked: {d}");
    }

    #[test]
    fn context_radius_is_three_by_default() {
        // The renderer's `context_radius(3)` matches git's default
        // and the design's choice. A change would alter every
        // diff a user sees.
        let old = "1\n2\n3\n4\n5\n6\n7\n8\n9\n";
        let new = "1\n2\n3\n4\nX\n6\n7\n8\n9\n";
        let d = render_unified_diff(old, new, "f");
        // Three context lines before and after the change.
        assert!(d.contains(" 3\n"), "context line 3 missing: {d}");
        assert!(d.contains(" 4\n"), "context line 4 missing: {d}");
        assert!(d.contains(" 6\n"), "context line 6 missing: {d}");
        assert!(d.contains(" 7\n"), "context line 7 missing: {d}");
        // And one line farther out should not be shown.
        assert!(!d.contains(" 1\n"), "unexpected far context: {d}");
    }

    #[test]
    fn unicode_content_round_trips() {
        let old = "café\n";
        let new = "café — updated\n";
        let d = render_unified_diff(old, new, "f");
        let applied = apply_unified_diff(old, &d).unwrap();
        assert_eq!(applied, new);
    }
    #[test]
    fn two_adjacent_hunks_are_rendered_separately() {
        // A change at line 1 and a change at line 20 with no
        // overlap produce two hunks in one diff. Both must apply.
        let old: String = (1..=25).map(|i| format!("{i}\n")).collect();
        let mut new = String::new();
        for i in 1..=25 {
            if i == 1 {
                new.push_str("first-changed\n");
            } else if i == 20 {
                new.push_str("twentieth-changed\n");
            } else {
                new.push_str(&format!("{i}\n"));
            }
        }
        let d = render_unified_diff(&old, &new, "f");
        let applied = apply_unified_diff(&old, &d).unwrap();
        assert_eq!(applied, new);
    }
}
