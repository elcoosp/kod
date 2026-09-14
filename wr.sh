#!/usr/bin/env bash
set -uo pipefail

LOOP=crates/kod-tui/src/main_loop.rs

if [ ! -f Cargo.toml ] || [ ! -f "$LOOP" ]; then
    echo "ERROR: run from the kod workspace root ($LOOP missing)"
    exit 1
fi

echo "Patching $LOOP: save the session after each completed turn"

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
# 1. ResponseComplete: save after finish_response, gated on the same
#    persist_history flag that governs history persistence. Tests do
#    not set that flag, so they never touch the real session file.
# ----------------------------------------------------------------------
patch(
    '''            Event::ResponseComplete(text) => {
                self.gen_task = None;
                self.app.finish_response(&text);
            }''',
    '''            Event::ResponseComplete(text) => {
                self.gen_task = None;
                self.app.finish_response(&text);
                // Snapshot the transcript after every completed turn.
                // The doc on KodApp::save_session has always claimed
                // "called on quit / after each assistant reply", but
                // only the quit path was wired — a crash mid-session
                // lost every turn since startup, not just the current
                // one. The write is atomic (temp + rename), so
                // persisting this often is safe; the cost is one small
                // JSON write per turn, negligible next to the model
                // call that just finished.
                //
                // Gated on persist_history so tests, which never call
                // TuiLoop::run, do not touch the user's real
                // ~/.kod/tui_session.json.
                if self.persist_history {
                    self.app.save_session();
                }
            }''',
    "save_session after ResponseComplete",
)

# ----------------------------------------------------------------------
# 2. Cancelled and Error also terminate a turn. A user who hits Esc
#    halfway through a long generation still wants the partial answer
#    and everything before it to survive a crash. Save there too.
# ----------------------------------------------------------------------
patch(
    '''            Event::Cancelled => {
                self.gen_task = None;
                self.app.cancel_generation();
            }''',
    '''            Event::Cancelled => {
                self.gen_task = None;
                self.app.cancel_generation();
                // Cancelled turns are still worth persisting — the
                // partial assistant reply is kept (see
                // KodApp::cancel_generation), and the rest of the
                // session is unchanged. Same gate as ResponseComplete.
                if self.persist_history {
                    self.app.save_session();
                }
            }''',
    "save_session after Cancelled",
)

patch(
    '''            Event::Error(error) => {
                self.gen_task = None;
                self.app.fail_generation(&error);
            }''',
    '''            Event::Error(error) => {
                self.gen_task = None;
                self.app.fail_generation(&error);
                // A failed turn appends a system message and settles
                // any running tool rows. Persist so a restart resumes
                // from the recorded error rather than the state
                // before it.
                if self.persist_history {
                    self.app.save_session();
                }
            }''',
    "save_session after Error",
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
fix(tui): persist the session after every turn, not just on quit

KodApp::save_session's doc comment promised "called on quit / after
each assistant reply", but only TuiLoop::run wired the quit side.
A crash, a terminal killed by the OS, or a `pkill` from another
shell mid-session lost every turn since the process started — not
the current turn, the entire conversation. The user had a chat
they could see on screen, no file on disk that matched it, and no
way to recover.

Save after every terminal event of the agentic loop:

  * ResponseComplete — the normal end of a turn.
  * Cancelled        — a partial reply was kept and the rest of the
                       session is unchanged; it should survive too.
  * Error            — a system message was appended and any running
                       tool row settled; a restart should resume from
                       that state, not the previous one.

All three are gated on the same `persist_history` flag that governs
prompt-history writes: it is set to true only by TuiLoop::run, so
tests that drive handle_event directly still never touch the user's
real ~/.kod/tui_session.json.

The cost is one small JSON write per turn. The write is
temp-file-plus-rename (see the earlier save_session fix), so it
cannot corrupt an existing session file if the process dies
mid-write.
MSG
