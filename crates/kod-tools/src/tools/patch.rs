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
use similar::{ChangeTag, TextDiff};

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
                HunkLine::Context(text) => {
                    match lines.get(cursor) {
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
                    }
                }
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
    let inner = line
        .trim_start_matches("@@")
        .trim_end_matches("@@")
        .trim();
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
        None => Ok((s.parse().map_err(|_| KodError::InvalidParameters {
            reason: format!("bad range: {s}"),
        })?, 1)),
    }
}

/// Render a unified diff between `old` and `new`.
pub fn render_unified_diff(old: &str, new: &str, path: &str) -> String {
    let diff = TextDiff::from_lines(old, new);
    let mut out = String::new();
    out.push_str(&format!("--- a/{path}\n"));
    out.push_str(&format!("+++ b/{path}\n"));
    for (i, group) in diff.grouped_ops(3).iter().enumerate() {
        if i > 0 {
            out.push_str("@@ ... @@\n");
        }
        for op in group {
            for change in diff.iter_changes(op) {
                let sign = match change.tag() {
                    ChangeTag::Delete => "-",
                    ChangeTag::Insert => "+",
                    ChangeTag::Equal => " ",
                };
                out.push_str(sign);
                out.push_str(change.value());
                if !change.value().ends_with('\n') {
                    out.push('\n');
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_simple_replacement() {
        let original = "line one\nline two\nline three\n";
        let patch = "--- a/f\n+++ b/f\n@@ -1,3 +1,3 @@\n line one\n-line two\n+LINE TWO\n line three\n";
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
