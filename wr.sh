#!/usr/bin/env bash
set -uo pipefail

LOOP=crates/kod-tui/src/main_loop.rs

if [ ! -f Cargo.toml ] || [ ! -f "$LOOP" ]; then
    echo "ERROR: run from the kod workspace root ($LOOP missing)"
    exit 1
fi

echo "Patching $LOOP: begin_generation before note_prompt"

python3 - "$LOOP" << 'PYEOF'
import os
import sys

target = sys.argv[1]
with open(target, "r") as f:
    src = f.read()

old = '''        // Track session context before the prompt leaves the TUI.
        self.app.note_prompt(&input);

        // Show the thinking indicator until the response lands. This also
        // arms the streaming accumulator so ResponseChunk events are kept.
        self.app.begin_generation();'''

new = '''        // Mark the turn as started BEFORE counting the prompt.
        //
        // begin_generation resets `turn_has_real_usage`, which
        // note_prompt's estimate consults via note_usage. The previous
        // order (note_prompt, then begin_generation) meant that on any
        // turn after a turn that received a real TokenUsage total, the
        // flag was still true from the previous turn when note_prompt
        // ran, so the prompt's estimate was silently dropped and the
        // meter under-reported until the next TokenUsage arrived.
        //
        // begin_generation also arms the streaming accumulator, so
        // moving it up does not change event handling — it just
        // establishes "new turn" before anything contributes to the
        // turn's accounting.
        self.app.begin_generation();

        // Track session context after the turn boundary is set.
        self.app.note_prompt(&input);'''

n = src.count(old)
if n != 1:
    print(f"ERROR: expected 1 occurrence of the prompt/generation block, found {n}")
    sys.exit(2)
src = src.replace(old, new, 1)

tmp = target + ".tmp"
with open(tmp, "w") as f:
    f.write(src)
os.replace(tmp, target)
print("Patched", target)
PYEOF

if [ $? -ne 0 ]; then
    echo "ERROR: patching failed"
    exit 1
fi

echo
echo "cargo check --workspace --all-targets 2>&1 | tail -20"
if ! cargo check --workspace --all-targets 2>&1 | tail -20; then
    echo "Compilation failed"
    exit 1
fi

echo
echo "cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -20"
if ! cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -20; then
    echo "Clippy failed"
    exit 1
fi

echo
echo "Committing."
git add -A
git commit -F - <<'MSG'
fix(tui): begin_generation before note_prompt in dispatch_prompt

The previous commit (real usage suppresses further char estimates)
introduced an ordering bug that only shows after the first turn:

  dispatch_prompt called note_prompt, then begin_generation.
  begin_generation resets turn_has_real_usage to false. On a turn
  after a turn that received a real TokenUsage event, the flag was
  still true when note_prompt ran, so note_usage was a no-op for the
  new prompt. begin_generation then reset the flag — but the
  estimate had already been dropped.

Result: the first turn of a session was counted, every subsequent
turn's prompt was not. The context meter lagged reality by the
length of the current prompt until TokenUsage arrived, and
auto-compact could fire a turn late.

Swap the two calls: begin_generation first (new turn, flag reset),
then note_prompt (counts under the fresh turn). No other behaviour
changes; begin_generation also arms the streaming accumulator, which
does not depend on note_prompt's state.
MSG
