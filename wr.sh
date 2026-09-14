#!/usr/bin/env bash
set -uo pipefail

ENGINE=crates/kod-core/src/engine.rs

echo "=== Diagnostic: final_text.push_str call sites ==="
grep -n "final_text.push_str" "$ENGINE"

echo
echo "=== Diagnostic: thinking_marker emission in run_streaming_loop ==="
grep -n "thinking_marker()" "$ENGINE"

echo
echo "Patching $ENGINE"

python3 - "$ENGINE" << 'PYEOF'
import os
import sys

target = sys.argv[1]
with open(target, "r") as f:
    src = f.read()

def patch(old, new, label, expect=1):
    global src
    n = src.count(old)
    if n == 0:
        print(f"  SKIP (anchor absent): {label}")
        return False
    if expect and n != expect:
        print(f"  ERROR: expected {expect} occurrence(s) of {label}, found {n}")
        sys.exit(2)
    src = src.replace(old, new, expect if expect else n)
    print(f"  patched: {label}")
    return True

# ----------------------------------------------------------------------
# 1. Add the helper next to truncate_chars.
# ----------------------------------------------------------------------
if "fn append_round_text" not in src:
    patch(
        '''pub(crate) fn truncate_chars(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}''',
        '''pub(crate) fn truncate_chars(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Append a round's text to the accumulated final text, inserting a
/// blank-line separator when this is not the first non-empty round.
///
/// Within one `process*` call the agentic loop can produce text in
/// more than one round: the model writes a sentence, calls a tool,
/// then writes a follow-up. Each round's text went into the same
/// `final_text` and, without a separator, the caller saw
/// "Let me check.Here is the answer." — two sentences jammed into
/// one. The TUI side-stepped the visible join because it flushes
/// each round into its own bubble (see `flush_streamed_text`), but a
/// non-streaming caller (the returned `final_text`) or a stream
/// consumer that does not track round boundaries saw the run-together
/// text.
fn append_round_text(buf: &mut String, text: &str) {
    if text.is_empty() {
        return;
    }
    if !buf.is_empty() {
        buf.push_str("\\n\\n");
    }
    buf.push_str(text);
}''',
        "append_round_text helper",
    )
else:
    print("  helper already present")

# ----------------------------------------------------------------------
# 2. run_collected_loop: replace the two push_str(&content) sites.
# ----------------------------------------------------------------------
if "append_round_text(&mut final_text, &content)" not in src:
    patch(
        '''                    last_usage = usage.or(last_usage);
                    final_text.push_str(&content);
                    break;
                }
                GenerationResponse::ToolCalls { calls, usage } => {''',
        '''                    last_usage = usage.or(last_usage);
                    append_round_text(&mut final_text, &content);
                    break;
                }
                GenerationResponse::ToolCalls { calls, usage } => {''',
        "collected loop: Text arm",
    )
    patch(
        '''                    last_usage = usage.or(last_usage);
                    final_text.push_str(&content);
                    if calls.is_empty() {
                        break;
                    }
                    let section = self.run_tool_calls(&calls).await;''',
        '''                    last_usage = usage.or(last_usage);
                    append_round_text(&mut final_text, &content);
                    if calls.is_empty() {
                        break;
                    }
                    let section = self.run_tool_calls(&calls).await;''',
        "collected loop: Mixed arm",
    )
else:
    print("  collected loop already uses append_round_text")

# ----------------------------------------------------------------------
# 3. run_streaming_loop: replace the push_str(&text) site.
# ----------------------------------------------------------------------
if "append_round_text(&mut final_text, &text)" not in src:
    patch(
        '''            last_usage = usage.or(last_usage);
            final_text.push_str(&text);
            if calls.is_empty() {
                break;
            }''',
        '''            last_usage = usage.or(last_usage);
            append_round_text(&mut final_text, &text);
            if calls.is_empty() {
                break;
            }''',
        "streaming loop: text accumulation",
    )
else:
    print("  streaming loop already uses append_round_text")

# ----------------------------------------------------------------------
# 4. run_streaming_loop: send a separator into the chunk stream before
#    the next round begins, so a consumer that prints chunks straight
#    through (the CLI) sees the round boundary. The TUI trims leading
#    blank lines on flush, so this is a no-op for it.
# ----------------------------------------------------------------------
if "kod-round-separator" not in src:
    patch(
        '''            tool_calls.extend(calls);
            tool_results.extend(section.results);
            pending.push_str(&format!("\\n\\n{}", section.prompt_block));
            self.apply_steers(pending).await;
            // Tool is done, result reinjected — next provider call is pure
            // LLM thinking, not tool execution. Tell the UI to drop the
            // "tool: …" line so a slow model doesn't look like a stuck tool.
            let _ = chunk_tx.send(thinking_marker()).await;''',
        '''            tool_calls.extend(calls);
            tool_results.extend(section.results);
            pending.push_str(&format!("\\n\\n{}", section.prompt_block));
            self.apply_steers(pending).await;
            // If we have already produced text this turn, emit a
            // blank-line separator into the chunk stream before the
            // next round. A consumer that prints chunks straight
            // through (the CLI's `kod chat`) would otherwise see the
            // two rounds' text jammed into one sentence. The TUI
            // trims leading blank lines on flush (see
            // `trim_blank_lines`), so this is a no-op for it.
            if !final_text.is_empty() {
                let _ = chunk_tx.send("\\n\\n".to_string()).await; // kod-round-separator
            }
            // Tool is done, result reinjected — next provider call is pure
            // LLM thinking, not tool execution. Tell the UI to drop the
            // "tool: …" line so a slow model doesn't look like a stuck tool.
            let _ = chunk_tx.send(thinking_marker()).await;''',
        "streaming loop: separator chunk between rounds",
    )
else:
    print("  separator chunk already present")

# ----------------------------------------------------------------------
# 5. Unit test for the helper.
# ----------------------------------------------------------------------
if "test_append_round_text_separates_rounds" not in src:
    anchor = '''    #[test]
    fn test_truncate_chars_respects_boundaries() {'''
    if anchor not in src:
        print("  ERROR: test anchor not found")
        sys.exit(2)
    new_test = '''    /// A single turn's agentic loop can produce text in more than one
    /// round (model says something, calls a tool, then says more).
    /// `append_round_text` must insert a blank-line separator between
    /// non-empty rounds so the accumulated `final_text` does not read
    /// as "Let me check.Here is the answer."
    #[test]
    fn test_append_round_text_separates_rounds() {
        let mut buf = String::new();
        // First round: no separator.
        append_round_text(&mut buf, "first");
        assert_eq!(buf, "first");
        // Second round: blank-line separator.
        append_round_text(&mut buf, "second");
        assert_eq!(buf, "first\\n\\nsecond");
        // Empty text is ignored — no separator for a round that
        // produced nothing.
        append_round_text(&mut buf, "");
        assert_eq!(buf, "first\\n\\nsecond");
        // Third round: separator again.
        append_round_text(&mut buf, "third");
        assert_eq!(buf, "first\\n\\nsecond\\n\\nthird");
        // Empty buffer + empty text: stays empty.
        let mut empty = String::new();
        append_round_text(&mut empty, "");
        assert_eq!(empty, "");
        // Empty buffer + first text: no leading separator.
        append_round_text(&mut empty, "one");
        assert_eq!(empty, "one");
    }

    #[test]
    fn test_truncate_chars_respects_boundaries() {'''
    src = src.replace(anchor, new_test, 1)
    print("  added test_append_round_text_separates_rounds")
else:
    print("  test already present")

tmp = target + ".tmp"
with open(tmp, "w") as f:
    f.write(src)
os.replace(tmp, target)
print("Wrote", target)
PYEOF

if [ $? -ne 0 ]; then
    echo "ERROR: patching failed"
    exit 1
fi

echo
echo "cargo check --workspace --all-targets 2>&1 | tail -12"
if ! cargo check --workspace --all-targets 2>&1 | tail -12; then
    echo "Compilation failed"
    exit 1
fi

echo
echo "cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -12"
if ! cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -12; then
    echo "Clippy failed"
    exit 1
fi

cat > /tmp/kod_commit_msg.txt <<'MSG'
fix(core): separate rounds' text in the agentic loop

Within one process* call the agentic loop can produce text in more
than one round: the model writes a sentence, calls a tool, then
writes a follow-up after the tool result comes back. Each round's
text was appended to the same final_text with no separator, so a
non-streaming caller saw "Let me check.Here is the answer." — two
sentences jammed into one, with no indication a tool round had
happened in between.

The TUI side-stepped the visible join because it flushes each
round into its own bubble (flush_streamed_text at ToolStarted), and
finish_response then skips the concatenated final_text once a
bubble has been flushed. But two consumers saw the bug:

  - The CLI's `kod chat` prints streamed chunks straight through
    with no round awareness, so a multi-round reply read as a
    single run-on paragraph.
  - Any caller of process / process_streaming / process_goal_streaming
    that used the returned final_text for display or storage got the
    concatenated form.

Fix in two places:

1. append_round_text(buf, text) inserts "\n\n" between non-empty
   rounds. Both loops (run_collected_loop, run_streaming_loop) use
   it in place of the raw push_str. This makes the returned
   final_text well-formed.

2. run_streaming_loop sends "\n\n" into the chunk stream before the
   thinking_marker that separates rounds, so a consumer that prints
   chunks straight through (the CLI) sees the boundary. The TUI
   trims leading blank lines on flush (trim_blank_lines), so this is
   a no-op for it.

The goal loop's turn-separation logic is unaffected — it already
inserted "\n\n" between turns; the fix is within each turn.

Adds test_append_round_text_separates_rounds, covering first-round
no-separator, subsequent-round separator, empty-text ignore, and
the empty-buffer edge cases.
MSG

git add -A
git commit -F /tmp/kod_commit_msg.txt
rm -f /tmp/kod_commit_msg.txt
