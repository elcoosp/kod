#!/usr/bin/env bash
set -uo pipefail

TOOLS=crates/kod-tools/src/tools.rs
EVENT=crates/kod-tui/src/event.rs

echo "=== event.rs:180-205 ==="
sed -n '180,205p' "$EVENT"

echo
echo "Patching $TOOLS (the actual select! block)"

python3 - "$TOOLS" "$EVENT" << 'PYEOF'
import os
import sys

tools, event = sys.argv[1], sys.argv[2]

def patch(path, old, new, label, expect=1):
    with open(path, "r") as f:
        src = f.read()
    n = src.count(old)
    if n == 0:
        print(f"  SKIP (anchor absent): {label}")
        return False
    if expect and n != expect:
        print(f"  ERROR: expected {expect} occurrence(s) of {label} in {path}, found {n}")
        sys.exit(2)
    tmp = path + ".tmp"
    with open(tmp, "w") as f:
        f.write(src.replace(old, new, expect if expect else n))
    os.replace(tmp, path)
    print(f"  patched {path}: {label}")
    return True

# ----------------------------------------------------------------------
# tools.rs: remove `&& !timed_out` from the two read arms. Keep the
# guard on the timeout arm.
# ----------------------------------------------------------------------
patch(
    tools,
    '''        loop {
            if stdout_res.is_some() && stderr_res.is_some() {
                break;
            }
            // No timed_out handling here: when the timeout branch fires
            // we call start_kill, the child dies, and the child's death
            // closes its pipe ends — so the two read arms complete on
            // their own and the loop exits through the top-of-loop
            // check above. (The previous version carried an empty
            // `if timed_out {}` block whose only content was a comment
            // explaining that fact.)
            tokio::select! {
                r = &mut stdout_fut, if stdout_res.is_none() && !timed_out => {
                    let over_cap = matches!(&r, Ok((_, true)));
                    stdout_res = Some(r);
                    if over_cap && stderr_res.is_none() {
                        let _ = child.start_kill();
                    }
                }
                r = &mut stderr_fut, if stderr_res.is_none() && !timed_out => {
                    let over_cap = matches!(&r, Ok((_, true)));
                    stderr_res = Some(r);
                    if over_cap && stdout_res.is_none() {
                        let _ = child.start_kill();
                    }
                }
                _ = &mut timeout, if !timed_out => {
                    timed_out = true;
                    let _ = child.start_kill();
                }
            }
        }''',
    '''        loop {
            if stdout_res.is_some() && stderr_res.is_some() {
                break;
            }
            // The two read arms are gated only on "this read has not
            // finished yet" — NOT on `!timed_out`. The previous code
            // included `!timed_out` in every read guard, so the
            // moment the timeout arm fired (set `timed_out = true`,
            // killed the child), the next loop iteration reached
            // `tokio::select!` with all three arms disabled and no
            // `else` — which panics with "all branches are disabled
            // and there is no else branch".
            //
            // The panic fired on the exact case the timeout exists
            // for: a command that produces no output and never exits
            // (e.g. `sleep 9999`). The runaway-output test used `yes`
            // and never hit it, because a read arm always completed
            // before the timeout had a chance to fire.
            //
            // With the reads polled after a timeout: the child is
            // dead, its death closes the pipe write ends, and each
            // read returns whatever bytes were buffered followed by
            // EOF. The timeout arm keeps `!timed_out` so the sleep
            // fires exactly once.
            tokio::select! {
                r = &mut stdout_fut, if stdout_res.is_none() => {
                    let over_cap = matches!(&r, Ok((_, true)));
                    stdout_res = Some(r);
                    if over_cap && stderr_res.is_none() {
                        let _ = child.start_kill();
                    }
                }
                r = &mut stderr_fut, if stderr_res.is_none() => {
                    let over_cap = matches!(&r, Ok((_, true)));
                    stderr_res = Some(r);
                    if over_cap && stdout_res.is_none() {
                        let _ = child.start_kill();
                    }
                }
                _ = &mut timeout, if !timed_out => {
                    timed_out = true;
                    let _ = child.start_kill();
                }
            }
        }''',
    "tools.rs: drop !timed_out from read arms",
)

# ----------------------------------------------------------------------
# event.rs:190 — inspect and patch if all arms are guarded.
# ----------------------------------------------------------------------
with open(event, "r") as f:
    ev_src = f.read()

old_sel = '''        tokio::select! {
            event = rx.recv() => {
                if let Some(event) = event {
                    return event;
                }
            }
            _ = tokio::time::sleep_until(deadline) => {
                return Event::Tick;
            }
        }'''

if old_sel in ev_src:
    print("  event.rs: select! has an unguarded `rx.recv()` arm — no fix needed")
else:
    # Print the actual block so we can see what it looks like.
    idx = ev_src.find("tokio::select!")
    if idx >= 0:
        print("  event.rs select! block (first 400 chars):")
        print("    " + ev_src[idx:idx + 400].replace("\n", "\n    "))
    else:
        print("  event.rs: select! not found?")
PYEOF

if [ $? -ne 0 ]; then
    echo "ERROR: patching failed"
    exit 1
fi

echo
echo "cargo check --workspace --all-targets 2>&1 | tail -10"
if ! cargo check --workspace --all-targets 2>&1 | tail -10; then
    echo "Compilation failed"
    exit 1
fi

echo
echo "cargo test -p kod-tools execute_command 2>&1 | tail -18"
if ! cargo test -p kod-tools execute_command 2>&1 | tail -18; then
    echo "kod-tools tests failed"
    exit 1
fi

cat > /tmp/kod_commit_msg.txt <<'MSG'
fix(tools): execute_command no longer panics on timeout

The select! loop in ExecuteCommandTool::execute gated all three
arms on `!timed_out`:

  tokio::select! {
      r = &mut stdout_fut, if stdout_res.is_none() && !timed_out => { ... }
      r = &mut stderr_fut, if stderr_res.is_none() && !timed_out => { ... }
      _ = &mut timeout,    if !timed_out                       => { ... }
  }

When the timeout arm fired (set timed_out = true, killed the child)
and the loop came around to a fresh select!, all three guards were
false. `tokio::select!` with every arm disabled and no `else`
panics with "all branches are disabled and there is no else branch".

So a command that produces no output and never exits — the exact
case the timeout was added for, like `sleep 9999` — did not time
out cleanly; it panicked after the kill. The runaway-output test
did not catch this: `yes` produces output, so a read arm always
completed before the timeout had a chance to fire.

Remove `&& !timed_out` from the two read arms. The reads are polled
until they finish; the child is dead by then, its death closes the
pipe write ends, and each read returns whatever bytes were
buffered followed by EOF. The timeout arm keeps `!timed_out` so the
sleep fires exactly once.

Adds execute_command_times_out_without_panic: 1-second timeout,
`sleep 60`, assertions that the call returns within a few seconds
and reports timed_out: true with empty stdout. The panic this
replaces fails the test before any assertion; a missed timeout
blows past the elapsed bound.
MSG

git add -A
git commit -F /tmp/kod_commit_msg.txt
rm -f /tmp/kod_commit_msg.txt
