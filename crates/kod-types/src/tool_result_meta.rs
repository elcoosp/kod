//! Delta §7.7 item 1: structured truncation meta + the `useless` flag.
//!
//! # Why structured, not a prose marker
//!
//! A truncated tool result today ends with a prose marker
//! (`… [truncated 90000 bytes]`). That tells a reader it was cut but
//! not *what to do next*: which direction the cut went, whether a
//! continuation exists, or which artifact holds the full body. A
//! structured [`TruncationMeta`] carries those, so the render path (and
//! a future "read the rest" tool) can act instead of re-running the
//! call.
//!
//! # The `useless` flag
//!
//! A tool result that provided nothing — an empty read, a grep with no
//! hits, a command that printed only its own banner — is noise in the
//! transcript and a target for §3 pruning. [`is_useless`] is the cheap
//! heuristic; the flag lives on `MessageMetadata` so a pruning pass
//! can drop the message without re-inspecting the result.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Which end of a body was cut.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TruncationDirection {
    /// The tail was dropped; the head survives.
    Tail,
    /// The head was dropped; the tail survives.
    Head,
    /// Both ends were dropped; a middle window survives.
    Middle,
}

/// Structured metadata for a truncated tool result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TruncationMeta {
    /// Which end was cut.
    pub direction: TruncationDirection,
    /// Byte offset in the *original* body where the returned slice
    /// starts. A continuation reads from `next_offset` with the same
    /// limits.
    pub next_offset: usize,
    /// Total bytes in the original body.
    pub original_bytes: usize,
    /// The cap that forced the truncation, in bytes.
    pub limit_bytes: usize,
    /// The artifact id holding the full body, when the minimizer or an
    /// offload hook stored one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_id: Option<String>,
}

impl TruncationMeta {
    /// Bytes actually returned.
    pub fn returned_bytes(&self) -> usize {
        self.original_bytes.saturating_sub(self.limit_bytes)
    }

    /// True when there is more to read (`next_offset` is before the
    /// end).
    pub fn has_continuation(&self) -> bool {
        self.next_offset < self.original_bytes
    }
}

/// A short human-readable suffix for the returned text. Kept terse —
/// the structured form is on the result, this is the inline hint.
pub fn truncation_note(meta: &TruncationMeta) -> String {
    let more = if meta.has_continuation() {
        format!("; continue at offset {}", meta.next_offset)
    } else {
        String::new()
    };
    match meta.direction {
        TruncationDirection::Tail => format!(
            "… [truncated {} of {} bytes{more}]",
            meta.returned_bytes(),
            meta.original_bytes,
        ),
        TruncationDirection::Head => format!(
            "… [head truncated; showing last {} of {} bytes{more}]",
            meta.returned_bytes(),
            meta.original_bytes,
        ),
        TruncationDirection::Middle => format!(
            "… [middle window of {} bytes from a {} byte body{more}]",
            meta.returned_bytes(),
            meta.original_bytes,
        ),
    }
}

/// A cheap "this result told the model nothing" heuristic, keyed on
/// the tool name and the result payload.
///
/// The cases that matter in practice:
///
/// * A `read_file` of an empty file (zero-length content).
/// * A `grep` / `list_files` with zero results.
/// * A `git_status` / `git_diff` with no changes.
/// * Any `Success` whose payload is an empty object or array.
///
/// Deliberately conservative: a result that carries *any* non-empty
/// field is not useless. The cost of a false negative (an empty result
/// kept) is small; the cost of a false positive (a useful result
/// dropped) is a wrong prune.
pub fn is_useless(tool_name: &str, result: &Value) -> bool {
    // An error result is not "useless" — it carries a message the
    // model must see.
    if result.get("error").is_some() {
        return false;
    }
    // Tool-specific empty checks.
    match tool_name {
        "grep" => {
            return result
                .get("results")
                .and_then(|v| v.as_array())
                .is_some_and(|a| a.is_empty());
        }
        "list_files" => {
            return result
                .get("files")
                .and_then(|v| v.as_array())
                .is_some_and(|a| a.is_empty());
        }
        "read_file" => {
            return result
                .get("content")
                .and_then(|v| v.as_str())
                .is_some_and(|s| s.trim().is_empty());
        }
        "git_status" | "git_diff" => {
            if let Some(d) = result.get("diff").and_then(|v| v.as_str())
                && d.trim().is_empty()
            {
                return true;
            }
            return result
                .get("changes")
                .and_then(|v| v.as_array())
                .is_some_and(|a| a.is_empty());
        }
        _ => {}
    }
    // A completely empty payload (object or array) is useless for any
    // tool.
    match result {
        Value::Object(m) => m.is_empty(),
        Value::Array(a) => a.is_empty(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn direction_serializes_lowercase() {
        let j = serde_json::to_string(&TruncationDirection::Tail).unwrap();
        assert_eq!(j, "\"tail\"");
    }

    #[test]
    fn a_tail_truncation_reports_returned_bytes() {
        let m = TruncationMeta {
            direction: TruncationDirection::Tail,
            next_offset: 1000,
            original_bytes: 5000,
            limit_bytes: 4000,
            artifact_id: None,
        };
        // 5000 - 4000 = 1000 returned; next offset is 1000.
        assert_eq!(m.returned_bytes(), 1000);
        assert!(m.has_continuation());
    }

    #[test]
    fn a_complete_slice_has_no_continuation() {
        let m = TruncationMeta {
            direction: TruncationDirection::Tail,
            next_offset: 5000,
            original_bytes: 5000,
            limit_bytes: 0,
            artifact_id: None,
        };
        assert!(!m.has_continuation());
    }

    #[test]
    fn the_note_names_the_direction() {
        let m = TruncationMeta {
            direction: TruncationDirection::Tail,
            next_offset: 100,
            original_bytes: 1000,
            limit_bytes: 900,
            artifact_id: None,
        };
        let n = truncation_note(&m);
        assert!(n.contains("continue at offset 100"), "got: {n}");
        assert!(n.contains("truncated"), "got: {n}");
    }

    #[test]
    fn head_and_middle_notes_differ() {
        let head = TruncationMeta {
            direction: TruncationDirection::Head,
            next_offset: 0,
            original_bytes: 100,
            limit_bytes: 90,
            artifact_id: None,
        };
        let mid = TruncationMeta {
            direction: TruncationDirection::Middle,
            next_offset: 0,
            original_bytes: 100,
            limit_bytes: 90,
            artifact_id: None,
        };
        assert!(truncation_note(&head).contains("head"));
        assert!(truncation_note(&mid).contains("middle"));
    }

    #[test]
    fn an_error_result_is_not_useless() {
        assert!(!is_useless("read_file", &json!({"error": "boom"})));
    }

    #[test]
    fn an_empty_read_is_useless() {
        assert!(is_useless("read_file", &json!({"content": "   \n"})));
        assert!(is_useless("read_file", &json!({"content": ""})));
    }

    #[test]
    fn a_nonempty_read_is_not_useless() {
        assert!(!is_useless(
            "read_file",
            &json!({"content": "fn main() {}"})
        ));
    }

    #[test]
    fn a_grep_with_no_hits_is_useless() {
        assert!(is_useless("grep", &json!({"results": []})));
        assert!(!is_useless("grep", &json!({"results": [{"file": "a"}]})));
    }

    #[test]
    fn an_empty_object_is_useless_for_any_tool() {
        assert!(is_useless("some_tool", &json!({})));
        assert!(is_useless("some_tool", &json!([])));
        assert!(!is_useless("some_tool", &json!({"x": 1})));
    }

    #[test]
    fn a_clean_git_diff_is_useless() {
        assert!(is_useless("git_diff", &json!({"diff": "  "})));
        assert!(!is_useless(
            "git_diff",
            &json!({"diff": "diff --git a/x b/x"})
        ));
    }
}
