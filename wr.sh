#!/usr/bin/env bash
set -uo pipefail

COMPILE_OK=true
TARGET=crates/kod-core/src/engine.rs

if [ ! -f Cargo.toml ] || [ ! -f "$TARGET" ]; then
    echo "ERROR: run from the kod workspace root ($TARGET missing)"
    exit 1
fi

echo "Patching $TARGET: line-anchored GOAL MET detection"

python3 - "$TARGET" << 'PYEOF'
import os
import sys

target = sys.argv[1]
with open(target, "r") as f:
    content = f.read()

def patch(old, new, label, expect=1):
    global content
    n = content.count(old)
    if n == 0:
        print(f"ERROR: old snippet not found: {label}")
        sys.exit(2)
    if expect and n != expect:
        print(f"ERROR: expected {expect} occurrence(s) of {label}, found {n}")
        sys.exit(2)
    content = content.replace(old, new, expect if expect else n)
    print(f"Patched: {label}")

# --- 1. Add a helper next to the other marker helpers ----------------
patch(
    '''/// Truncate a UTF-8 string to at most `max` bytes, rounding down to the
/// nearest char boundary. Returns the input unchanged when it already
/// fits. Use this instead of `&s[..max]` — the raw slice panics when
/// `max` lands mid-codepoint, which any non-ASCII tool output can hit
/// (a file containing "café", an error message with an em-dash, any
/// emoji in a directory listing).
pub(crate) fn truncate_chars(s: &str, max: usize) -> &str {''',
    '''/// Does this reply declare the goal met?
///
/// The goal prompt instructs the model to "end your reply with a line
/// containing exactly GOAL MET". The check that used to stand here was
/// `final_text.to_uppercase().contains("GOAL MET")` — a substring test
/// that fired on "I have not reached GOAL MET yet" and on "I cannot
/// determine if the GOAL MET criteria are satisfied", stopping the
/// loop with a false success and hiding the model's actual progress.
///
/// Anchor on the last non-empty line instead. Trim and strip the same
/// decorations a model often adds (`**GOAL MET**`, `GOAL MET.`,
/// `— GOAL MET`, `> GOAL MET`), then compare case-insensitively to
/// `GOAL MET`. A reply that only mentions the phrase mid-paragraph is
/// not a completion signal.
pub(crate) fn reply_declares_goal_met(text: &str) -> bool {
    let last_line = text
        .lines()
        .rev()
        .map(|l| l.trim())
        .find(|l| !l.is_empty());
    let Some(line) = last_line else {
        return false;
    };
    // Strip surrounding emphasis and leading quote / list markers.
    let stripped: String = line
        .trim_matches(|c: char| {
            c.is_whitespace() || c == '*' || c == '`' || c == '>' || c == '-'
        })
        .trim_start_matches(|c: char| c.is_whitespace() || c == '—' || c == ':')
        .trim_end_matches(|c: char| c.is_whitespace() || c == '.' || c == '!' || c == ':')
        .to_string();
    stripped.eq_ignore_ascii_case("GOAL MET")
}

/// Truncate a UTF-8 string to at most `max` bytes, rounding down to the
/// nearest char boundary. Returns the input unchanged when it already
/// fits. Use this instead of `&s[..max]` — the raw slice panics when
/// `max` lands mid-codepoint, which any non-ASCII tool output can hit
/// (a file containing "café", an error message with an em-dash, any
/// emoji in a directory listing).
pub(crate) fn truncate_chars(s: &str, max: usize) -> &str {''',
    "reply_declares_goal_met helper",
)

# --- 2. Use the helper in process_goal_streaming --------------------
patch(
    '''                if final_text.to_uppercase().contains("GOAL MET") {
                    break;
                }''',
    '''                if reply_declares_goal_met(&final_text) {
                    break;
                }''',
    "process_goal_streaming uses anchored check",
)

# --- 3. Tests -------------------------------------------------------
patch(
    '''    /// set_provider must complete promptly even while a streaming''',
    '''    /// `reply_declares_goal_met` must fire on the completion form and
    /// NOT fire on a mention of the phrase mid-reply. Regression: the
    /// previous substring check stopped the loop on "I have NOT
    /// reached GOAL MET".
    #[test]
    fn test_reply_declares_goal_met() {
        // The prompt's contract: last line is exactly GOAL MET.
        assert!(reply_declares_goal_met("Here is the summary.\\nGOAL MET"));
        // A model that capitalizes differently on the last line is
        // still a completion — the check is case-insensitive.
        assert!(reply_declares_goal_met("done\\ngoal met"));
        // Trailing punctuation and emphasis are tolerated.
        assert!(reply_declares_goal_met("…\\nGOAL MET."));
        assert!(reply_declares_goal_met("…\\n**GOAL MET**"));
        assert!(reply_declares_goal_met("…\\n— GOAL MET"));
        // Trailing blank lines after the marker are fine.
        assert!(reply_declares_goal_met("GOAL MET\\n\\n"));

        // A false promise does NOT declare success.
        assert!(!reply_declares_goal_met(
            "I have not reached GOAL MET yet, but I'm close."
        ));
        // The phrase on a non-final line is not a signal.
        assert!(!reply_declares_goal_met(
            "GOAL MET is what I'd say if done.\\nBut I need one more turn."
        ));
        // A question about the criteria is not a completion.
        assert!(!reply_declares_goal_met(
            "Should I reply GOAL MET now, or keep working?"
        ));
        // Empty input is not a completion.
        assert!(!reply_declares_goal_met(""));
        assert!(!reply_declares_goal_met("   \\n\\n"));
    }

    /// set_provider must complete promptly even while a streaming''',
    "reply_declares_goal_met tests",
)

tmp = target + ".tmp"
with open(tmp, "w") as f:
    f.write(content)
os.replace(tmp, target)
print("Wrote", target)
PYEOF

if [ $? -ne 0 ]; then
    echo "ERROR: patching failed"
    exit 1
fi

echo "cargo check --workspace --all-targets"
if ! cargo check --workspace --all-targets 2>&1; then
    echo "Compilation failed"
    exit 1
fi

echo "cargo clippy --workspace --all-targets -- -D warnings"
if ! cargo clippy --workspace --all-targets -- -D warnings 2>&1; then
    echo "Clippy failed"
    exit 1
fi

echo "Committing."
git add -A
git commit -F - <<'MSG'
fix(core): anchor GOAL MET detection to the last line

The goal loop decided the model was done with
`final_text.to_uppercase().contains("GOAL MET")` — a substring test
over the entire reply. It fired on

  "I have not reached GOAL MET yet, but I'm close."

and stopped the loop with a false success. It fired on

  "Should I reply GOAL MET now, or keep working?"

and abandoned a run that was still making progress. And it fired on
any mention of the phrase mid-paragraph, so a model writing a
summary that quoted the criterion ended the loop with nothing done.

The goal prompt already tells the model what the completion form
is: "end your reply with a line containing exactly GOAL MET". Match
that contract instead of scanning the text.

Add reply_declares_goal_met(text): take the last non-blank line,
strip surrounding whitespace, emphasis (** **, backticks), quote and
list markers (>), a leading em-dash or colon, and trailing
punctuation (. ! :), then compare case-insensitively to GOAL MET.
Anything else — including the phrase earlier in the reply, negated,
or in a question — does not declare success.

The test covers the acceptance and rejection forms and pins them;
the loop call site is a one-line swap.

No public API change beyond the new pub(crate) helper.
MSG
