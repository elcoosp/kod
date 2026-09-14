#!/usr/bin/env bash
set -uo pipefail

LOOP=crates/kod-tui/src/main_loop.rs

if [ ! -f Cargo.toml ] || [ ! -f "$LOOP" ]; then
    echo "ERROR: run from the kod workspace root ($LOOP missing)"
    exit 1
fi

echo "Patching $LOOP: implement /edit; add it to SLASH_HELP"

python3 - "$LOOP" << 'PYEOF'
import os
import sys

target = sys.argv[1]
with open(target, "r") as f:
    src = f.read()

def patch(old, new, label, expect=1):
    global src
    n = src.count(old)
    if n == 0:
        print(f"ERROR: anchor not found: {label}")
        sys.exit(2)
    if expect and n != expect:
        print(f"ERROR: expected {expect} occurrence(s) of {label}, found {n}")
        sys.exit(2)
    src = src.replace(old, new, expect if expect else n)
    print(f"Patched: {label}")

# ----------------------------------------------------------------------
# 1. Add /edit to SLASH_HELP, right after /undo (its conceptual
#    neighbour — both touch the previous message).
# ----------------------------------------------------------------------
patch(
    '\\n/undo — restore last /clear\\n/model',
    '\\n/undo — restore last /clear\\n/edit — load your last message back into the input for editing (also `e`)\\n/model',
    "SLASH_HELP adds /edit",
)

# ----------------------------------------------------------------------
# 2. Add the /edit handler in handle_command. Anchored after /undo,
#    whose arm is unique and short.
# ----------------------------------------------------------------------
patch(
    '''            "/undo" => {
                if self.app.undo_clear() {
                    self.app
                        .push_system_message("Restored last cleared messages.");
                } else {
                    self.app.push_system_message("Nothing to undo.");
                }
            }''',
    '''            "/undo" => {
                if self.app.undo_clear() {
                    self.app
                        .push_system_message("Restored last cleared messages.");
                } else {
                    self.app.push_system_message("Nothing to undo.");
                }
            }
            "/edit" => {
                // Same behaviour as the `e` keybinding: load the last
                // user message into the input box for editing. The
                // completion popup advertised /edit before this arm
                // existed, so picking it fell through to "Unknown
                // command: /edit" — a small lie caught by
                // test_slash_help_lists_every_command.
                if self.app.edit_last_message() {
                    self.app.set_input_mode(InputMode::Insert);
                    self.app.push_system_message(
                        "Loaded your last message for editing — press Enter to resend.",
                    );
                } else {
                    self.app
                        .push_system_message("Nothing to edit — no previous prompt.");
                }
            }''',
    "handle_command /edit arm",
)

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
echo "cargo test -p kod-tui --lib --quiet 2>&1 | tail -15"
if ! cargo test -p kod-tui --lib --quiet 2>&1 | tail -15; then
    echo "kod-tui lib tests failed"
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
fix(tui): implement /edit and document it; unblock the help invariant

SLASH_COMMANDS has advertised /edit (with the hint "edit your last
message again") since the autocomplete was introduced, but
handle_command had no /edit arm — picking it from the popup fell
through to "Unknown command: /edit". The behaviour exists (the `e`
keybinding calls edit_last_message and enters Insert mode); the
slash form simply had no implementation.

test_slash_help_lists_every_command, added earlier this session,
caught the drift: SLASH_HELP did not mention /edit either, so the
test failed on the missing entry. That is the second direction the
test covers — "each SLASH_COMMANDS entry appears in SLASH_HELP" —
doing its job.

Add the /edit arm to handle_command (edit_last_message + Insert
mode + a confirmation message; "Nothing to edit" when the history
is empty), and add the /edit line to SLASH_HELP, next to /undo —
its closest neighbour.
MSG
