//! Delta §14.4: map-reduce over a huge diff for commit proposals.
//!
//! Past a size threshold a diff does not fit in one prompt. The
//! fallback: split per file, summarize each (map), summarize the
//! summaries (reduce).
//!
//! The summarizer is injected — a `Fn(&str) -> String` — so the
//! orchestration is testable without a live model, and a caller wires
//! the real provider. This module owns the chunking and the reduce; it
//! does not own the model.

/// Past this many estimated tokens, a diff goes map-reduce.
pub const MAP_REDUCE_THRESHOLD_TOKENS: usize = 5_000;

/// A single file's diff over this many estimated tokens is truncated
/// before it reaches the summarizer.
pub const MAX_FILE_TOKENS: usize = 50_000;

/// The design's map concurrency (recorded; the orchestration here is
/// sequential and a caller parallelizes at the summarizer layer).
pub const MAP_CONCURRENCY: usize = 16;

/// Chars-per-token, the workspace convention.
const CHARS_PER_TOKEN: usize = 4;

/// One file's section of a diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    pub path: String,
    pub body: String,
}

/// Split a unified diff into per-file sections on `diff --git` lines.
/// A diff with no header becomes one section with an empty path.
pub fn split_by_file(diff: &str) -> Vec<FileDiff> {
    let mut out: Vec<FileDiff> = Vec::new();
    let mut current_path: Option<String> = None;
    let mut current_body = String::new();
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            if let Some(path) = current_path.take() {
                out.push(FileDiff {
                    path,
                    body: std::mem::take(&mut current_body),
                });
            }
            let path = rest
                .split_whitespace()
                .nth(1)
                .map(|p| p.trim_start_matches("b/").to_string())
                .unwrap_or_default();
            current_path = Some(path);
            current_body.push_str(line);
            current_body.push('\n');
            continue;
        }
        current_body.push_str(line);
        current_body.push('\n');
    }
    if let Some(path) = current_path {
        out.push(FileDiff {
            path,
            body: current_body,
        });
    }
    if out.is_empty() && !diff.trim().is_empty() {
        out.push(FileDiff {
            path: String::new(),
            body: diff.to_string(),
        });
    }
    out
}

fn est_tokens(s: &str) -> usize {
    s.len() / CHARS_PER_TOKEN
}

/// True when `diff` is large enough for the map-reduce path.
pub fn needs_map_reduce(diff: &str) -> bool {
    est_tokens(diff) > MAP_REDUCE_THRESHOLD_TOKENS
}

/// Truncate a file's diff body to [`MAX_FILE_TOKENS`], with a marker.
fn cap_file_body(body: &str) -> String {
    let max_chars = MAX_FILE_TOKENS * CHARS_PER_TOKEN;
    if body.len() <= max_chars {
        return body.to_string();
    }
    let mut end = max_chars;
    while end > 0 && !body.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n… [{} chars elided at MAX_FILE_TOKENS]",
        &body[..end],
        body.len() - end,
    )
}

/// Summarize each file section (map), then the per-file summaries
/// (reduce). `summarize` is the injected model call.
pub fn map_reduce(diff: &str, summarize: &dyn Fn(&str) -> String) -> String {
    if diff.trim().is_empty() {
        return String::new();
    }
    let files = split_by_file(diff);
    let mut per_file: Vec<String> = Vec::with_capacity(files.len());
    for f in &files {
        let body = cap_file_body(&f.body);
        let prompt = format!(
            "Summarize this file's change in one sentence. \
             Path: {}\n\n{body}",
            if f.path.is_empty() {
                "(unknown)"
            } else {
                &f.path
            },
        );
        let s = summarize(&prompt);
        per_file.push(format!(
            "{}: {}",
            if f.path.is_empty() {
                "(unknown)"
            } else {
                &f.path
            },
            s.trim(),
        ));
    }
    let joined = per_file.join("\n");
    summarize(&format!(
        "Combine these per-file change summaries into one paragraph \
         describing the whole commit:\n\n{joined}",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_by_file_reads_diff_git_headers() {
        let diff = "\
diff --git a/src/a.rs b/src/a.rs
index 1..2 100644
--- a/src/a.rs
+++ b/src/a.rs
@@ -1 +1 @@
-old
+new
diff --git a/src/b.rs b/src/b.rs
index 3..4 100644
--- a/src/b.rs
+++ b/src/b.rs
@@ -1 +1 @@
-x
+y
";
        let files = split_by_file(diff);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, "src/a.rs");
        assert_eq!(files[1].path, "src/b.rs");
        assert!(files[0].body.contains("+new"));
        assert!(!files[0].body.contains("src/b.rs"));
    }

    #[test]
    fn a_diff_with_no_headers_is_one_section() {
        let diff = "@@ -1 +1 @@\n-a\n+b\n";
        let files = split_by_file(diff);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "");
    }

    #[test]
    fn empty_diff_is_no_sections() {
        assert!(split_by_file("").is_empty());
    }

    #[test]
    fn needs_map_reduce_thresholds_on_size() {
        assert!(!needs_map_reduce(&"x".repeat(100)));
        let big = "x".repeat(MAP_REDUCE_THRESHOLD_TOKENS * CHARS_PER_TOKEN + 4);
        assert!(needs_map_reduce(&big));
    }

    #[test]
    fn cap_file_body_truncates_with_a_marker() {
        let body = "x".repeat(MAX_FILE_TOKENS * CHARS_PER_TOKEN + 100);
        let capped = cap_file_body(&body);
        assert!(capped.len() < body.len());
        assert!(capped.contains("elided at MAX_FILE_TOKENS"));
    }

    #[test]
    fn cap_file_body_leaves_a_small_body_alone() {
        assert_eq!(cap_file_body("small diff"), "small diff");
    }

    #[test]
    fn map_reduce_maps_each_file_then_reduces() {
        use std::sync::Mutex;
        let seen: Mutex<Vec<String>> = Mutex::new(Vec::new());
        let summarize = |p: &str| -> String {
            seen.lock().unwrap().push(p.to_string());
            if p.starts_with("Combine") {
                "REDUCED".to_string()
            } else {
                "file summary".to_string()
            }
        };
        let diff = "\
diff --git a/a.rs b/a.rs
@@ -1 +1 @@
-a
+b
diff --git a/b.rs b/b.rs
@@ -1 +1 @@
-c
+d
";
        let out = map_reduce(diff, &summarize);
        assert_eq!(out, "REDUCED");
        let calls = seen.lock().unwrap();
        assert_eq!(calls.len(), 3);
        assert!(calls[0].contains("Path: a.rs"));
        assert!(calls[1].contains("Path: b.rs"));
        assert!(calls[2].starts_with("Combine"));
        assert!(calls[2].contains("a.rs: file summary"));
        assert!(calls[2].contains("b.rs: file summary"));
    }

    #[test]
    fn map_reduce_on_empty_diff_is_empty() {
        let summarize = |_: &str| "unused".to_string();
        assert_eq!(map_reduce("", &summarize), "");
        assert_eq!(map_reduce("   \n", &summarize), "");
    }
}
