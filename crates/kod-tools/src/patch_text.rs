//! Delta §7.7 item 5: lift a `*** Begin Patch` payload out of
//! assistant text.
//!
//! A model that has learned the OpenAI `apply_patch` envelope will
//! sometimes emit one as prose — no tool call, just the text — and
//! then stop. Without a recovery step the edit is lost; the user
//! sees a reply that describes a change and no change.
//!
//! This module parses the envelope and converts each file section to
//! the unified-diff string [`crate::patch::apply_unified_diff`]
//! accepts. The engine turns each result into a synthetic
//! `patch_file` call, which runs through the normal approval path —
//! extraction is not an approval bypass.
//!
//! # Strict by construction
//!
//! The OpenAI format carries no line numbers: a hunk is an `@@`
//! marker, then context / `-` / `+` lines. Applying it means
//! *searching* for the context block. This module does a **strict
//! search**: the block must occur exactly once in the file. Zero
//! matches or several are both errors the caller surfaces.
//!
//! A fuzzy matcher (whitespace-tolerant, nearest-location scoring)
//! is the design's full intent, and it is where a bug silently edits
//! the wrong lines. A rejected patch the model can retry is strictly
//! better than a corrupted file, so the strict form is what ships; a
//! fuzzy layer can be added behind the same interface.
//!
//! # What this does NOT do
//!
//! * Not the engine hook. `extract` is pure; the caller decides when
//!   to run it and what to do with the result.
//! * Not an approval path. A caller that applies the diff without
//!   going through the tool layer bypasses approval — do not.

/// One file's change, in the parsed form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilePatch {
    /// Replace the whole file body with `content`.
    Add { path: String, content: String },
    /// Remove the file.
    Delete { path: String },
    /// Apply `hunks` (each a `@@` block of context/`-`/`+` lines) to
    /// the existing file.
    Update { path: String, hunks: Vec<Hunk> },
}

/// One `@@`-delimited block inside an `Update`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    /// The `@@` marker's trailing text (a section header, or empty).
    pub section: String,
    /// Lines of the hunk in order, each tagged by its first byte:
    /// `' '` context, `'-'` removal, `'+'` addition.
    pub lines: Vec<String>,
}

/// Parse the envelope `text` and return the file patches, or `None`
/// when `text` holds no `*** Begin Patch` … `*** End Patch` block.
///
/// A block that starts but does not end is also `None` (a truncated
/// reply is not a patch to apply half of). A block that parses but
/// names no file returns an empty vec, which the caller treats as
/// "nothing to do".
pub fn extract(text: &str) -> Option<Vec<FilePatch>> {
    let mut out: Vec<FilePatch> = Vec::new();
    let mut lines = text.lines();
    // Find the begin marker.
    let mut found_begin = false;
    let mut current: Option<FilePatch> = None;

    for line in lines.by_ref() {
        if line.trim() == "*** Begin Patch" {
            found_begin = true;
            break;
        }
    }
    if !found_begin {
        return None;
    }

    let mut saw_end = false;
    for line in lines {
        let trimmed = line.trim_end();
        if trimmed == "*** End Patch" {
            saw_end = true;
            break;
        }
        if let Some(rest) = trimmed.strip_prefix("*** Update File:") {
            if let Some(p) = current.take() {
                out.push(p);
            }
            current = Some(FilePatch::Update {
                path: rest.trim().to_string(),
                hunks: Vec::new(),
            });
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("*** Add File:") {
            if let Some(p) = current.take() {
                out.push(p);
            }
            current = Some(FilePatch::Add {
                path: rest.trim().to_string(),
                content: String::new(),
            });
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("*** Delete File:") {
            if let Some(p) = current.take() {
                out.push(p);
            }
            current = Some(FilePatch::Delete {
                path: rest.trim().to_string(),
            });
            continue;
        }
        // Body lines for the current section.
        match &mut current {
            Some(FilePatch::Add { content, .. }) => {
                // `+` prefix; anything else is tolerated as raw text.
                let body = line.strip_prefix('+').unwrap_or(line);
                content.push_str(body);
                content.push('\n');
            }
            Some(FilePatch::Update { hunks, .. }) => {
                if let Some(rest) = line.strip_prefix("@@") {
                    let section = rest.trim().to_string();
                    hunks.push(Hunk {
                        section,
                        lines: Vec::new(),
                    });
                    continue;
                }
                let Some(hunk) = hunks.last_mut() else {
                    // A body line before any `@@` — skip; a malformed
                    // hunk is not a patch.
                    continue;
                };
                // A hunk body line starts with ' ', '-', or '+'. A
                // blank line is a context line with its trailing
                // space stripped by the terminal or the model.
                if line.is_empty() {
                    hunk.lines.push(" ".to_string());
                } else if line.starts_with(' ')
                    || line.starts_with('-')
                    || line.starts_with('+')
                {
                    hunk.lines.push(line.to_string());
                }
                // Anything else: tolerated as a context line without
                // its prefix.
                else {
                    hunk.lines.push(format!(" {line}"));
                }
            }
            Some(FilePatch::Delete { .. }) => {}
            None => {}
        }
    }
    if let Some(p) = current.take() {
        out.push(p);
    }
    if !saw_end {
        return None;
    }
    Some(out)
}

/// Render one `FilePatch` as the unified-diff string `patch_file`
/// accepts, using `original` (the file's current content) to compute
/// hunk line numbers.
///
/// `Add` produces a `--- /dev/null` / `+++ b/<path>` diff of the whole
/// body. `Delete` produces the reverse. `Update` searches `original`
/// for each hunk's context block — the block must be unique, or the
/// function returns `Err` (see the module docs).
pub fn to_unified_diff(patch: &FilePatch, original: &str) -> Result<String, String> {
    match patch {
        FilePatch::Add { path, content } => {
            let mut out = String::new();
            out.push_str("--- /dev/null\n");
            out.push_str(&format!("+++ b/{path}\n"));
            out.push_str(&format!("@@ -0,0 +1,{} @@\n", content.lines().count()));
            for line in content.lines() {
                out.push('+');
                out.push_str(line);
                out.push('\n');
            }
            Ok(out)
        }
        FilePatch::Delete { path } => {
            let mut out = String::new();
            out.push_str(&format!("--- a/{path}\n"));
            out.push_str("+++ /dev/null\n");
            out.push_str(&format!("@@ -1,{} +0,0 @@\n", original.lines().count()));
            for line in original.lines() {
                out.push('-');
                out.push_str(line);
                out.push('\n');
            }
            Ok(out)
        }
        FilePatch::Update { path, hunks } => {
            let file_lines: Vec<&str> = original.split('\n').collect();
            let mut out = String::new();
            out.push_str(&format!("--- a/{path}\n"));
            out.push_str(&format!("+++ b/{path}\n"));
            for hunk in hunks {
                let (old_lines, new_lines) = split_hunk(&hunk.lines);
                let (start, count) = find_unique_block(&file_lines, &old_lines)
                    .ok_or_else(|| format!(
                        "hunk not found exactly once in {path}: \
                         search for {} context/removed line(s)",
                        old_lines.len()
                    ))?;
                let old_count = count;
                let new_count = new_lines.len();
                out.push_str(&format!(
                    "@@ -{start},{old_count} +{start},{new_count} @@\n"
                ));
                for l in &hunk.lines {
                    out.push_str(l);
                    out.push('\n');
                }
            }
            Ok(out)
        }
    }
}

/// Split a hunk's tagged lines into `(old_side, new_side)`. The old
/// side is context + removed; the new side is context + added. Used
/// to size the hunk header and to build the search key.
fn split_hunk(lines: &[String]) -> (Vec<String>, Vec<String>) {
    let mut old = Vec::new();
    let mut new = Vec::new();
    for l in lines {
        match l.as_bytes().first() {
            Some(b' ') => {
                let body = &l[1..];
                old.push(body.to_string());
                new.push(body.to_string());
            }
            Some(b'-') => old.push(l[1..].to_string()),
            Some(b'+') => new.push(l[1..].to_string()),
            _ => {}
        }
    }
    (old, new)
}

/// Find `needle` in `haystack` (as whole consecutive lines),
/// returning `(1-based start, count)` when it occurs exactly once.
///
/// `None` means zero matches or several; the caller turns either into
/// a rejection. This is the strict contract the module docs promise.
fn find_unique_block(haystack: &[&str], needle: &[String]) -> Option<(usize, usize)> {
    if needle.is_empty() {
        return None;
    }
    let mut found: Option<usize> = None;
    // Slide a window the width of `needle`.
    if haystack.len() < needle.len() {
        return None;
    }
    for start in 0..=(haystack.len() - needle.len()) {
        let mut matched = true;
        for (i, n) in needle.iter().enumerate() {
            // Compare on `trim_end` so a CRLF file (whose lines kept
            // no `\r` by the split above) matches a patch authored on
            // Unix, and a trailing-space difference is tolerated.
            if haystack[start + i].trim_end() != n.trim_end() {
                matched = false;
                break;
            }
        }
        if matched {
            if found.is_some() {
                // A second match: ambiguous, refuse.
                return None;
            }
            found = Some(start);
        }
    }
    found.map(|s| (s + 1, needle.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_returns_none_without_the_envelope() {
        assert!(extract("just prose, no patch").is_none());
        assert!(extract("*** Begin Patch but no end").is_none());
    }

    #[test]
    fn extract_parses_an_update_file() {
        let text = "Here is the fix:\n\
                    *** Begin Patch\n\
                    *** Update File: src/main.rs\n\
                    @@\n\
                     fn main() {\n\
                    -    old();\n\
                    +    new();\n\
                     }\n\
                    *** End Patch\n";
        let patches = extract(text).expect("a patch");
        assert_eq!(patches.len(), 1);
        match &patches[0] {
            FilePatch::Update { path, hunks } => {
                assert_eq!(path, "src/main.rs");
                assert_eq!(hunks.len(), 1);
                assert_eq!(hunks[0].lines.len(), 4);
            }
            other => panic!("expected Update, got {other:?}"),
        }
    }

    #[test]
    fn extract_parses_add_and_delete() {
        let text = "*** Begin Patch\n\
                    *** Add File: new.txt\n\
                    +line one\n\
                    +line two\n\
                    *** Delete File: old.txt\n\
                    *** End Patch\n";
        let patches = extract(text).expect("a patch");
        assert_eq!(patches.len(), 2);
        assert!(matches!(&patches[0], FilePatch::Add { path, .. } if path == "new.txt"));
        assert!(matches!(&patches[1], FilePatch::Delete { path } if path == "old.txt"));
    }

    #[test]
    fn update_converts_to_a_unified_diff() {
        let original = "fn main() {\n    old();\n}\n";
        let patch = FilePatch::Update {
            path: "src/main.rs".to_string(),
            hunks: vec![Hunk {
                section: String::new(),
                lines: vec![
                    " fn main() {".to_string(),
                    "-    old();".to_string(),
                    "+    new();".to_string(),
                    " }".to_string(),
                ],
            }],
        };
        let diff = to_unified_diff(&patch, original).expect("a diff");
        assert!(diff.contains("--- a/src/main.rs"));
        assert!(diff.contains("+++ b/src/main.rs"));
        assert!(diff.contains("@@ -1,3 +1,3 @@"));
        assert!(diff.contains("-    old();"));
        assert!(diff.contains("+    new();"));
        // The diff applies through the real applier.
        let applied = crate::patch::apply_unified_diff(original, &diff).expect("apply");
        assert!(applied.contains("new();"));
        assert!(!applied.contains("old();"));
    }

    #[test]
    fn update_rejects_a_block_that_does_not_appear() {
        let original = "fn main() {\n    old();\n}\n";
        let patch = FilePatch::Update {
            path: "f.rs".to_string(),
            hunks: vec![Hunk {
                section: String::new(),
                lines: vec!["     nope();".to_string()],
            }],
        };
        assert!(to_unified_diff(&patch, original).is_err());
    }

    #[test]
    fn update_rejects_an_ambiguous_block() {
        // The same three lines appear twice; a strict applier cannot
        // choose, so it refuses rather than editing the wrong one.
        let original = "a\nb\nc\nx\na\nb\nc\n";
        let patch = FilePatch::Update {
            path: "f.rs".to_string(),
            hunks: vec![Hunk {
                section: String::new(),
                lines: vec![
                    " a".to_string(),
                    " b".to_string(),
                    " c".to_string(),
                ],
            }],
        };
        assert!(to_unified_diff(&patch, original).is_err());
    }

    #[test]
    fn add_converts_to_a_new_file_diff() {
        let patch = FilePatch::Add {
            path: "new.txt".to_string(),
            content: "hello\nworld\n".to_string(),
        };
        let diff = to_unified_diff(&patch, "").expect("a diff");
        assert!(diff.contains("--- /dev/null"));
        assert!(diff.contains("+++ b/new.txt"));
        assert!(diff.contains("@@ -0,0 +1,2 @@"));
        assert!(diff.contains("+hello"));
    }

    #[test]
    fn delete_converts_to_a_removal_diff() {
        let patch = FilePatch::Delete {
            path: "old.txt".to_string(),
        };
        let diff = to_unified_diff(&patch, "one\ntwo\n").expect("a diff");
        assert!(diff.contains("--- a/old.txt"));
        assert!(diff.contains("+++ /dev/null"));
        assert!(diff.contains("-one"));
    }

    #[test]
    fn crlf_and_trailing_space_differences_are_tolerated() {
        // The file has CRLF; the patch was authored on Unix with no
        // trailing space. The strict match still finds it.
        let original = "fn main() {\r\n    old();\r\n}\r\n";
        let patch = FilePatch::Update {
            path: "f.rs".to_string(),
            hunks: vec![Hunk {
                section: String::new(),
                lines: vec![
                    " fn main() {".to_string(),
                    "-    old();".to_string(),
                    "+    new();".to_string(),
                ],
            }],
        };
        let diff = to_unified_diff(&patch, original).expect("a diff");
        assert!(diff.contains("@@ -1,2 +1,2 @@"));
    }
}
