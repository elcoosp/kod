#!/usr/bin/env bash
set -uo pipefail

TOOLS=crates/kod-tools/src/tools.rs

echo "=== Confirm read arms are unguarded ==="
grep -n "if stdout_res.is_none\|if stderr_res.is_none\|if !timed_out" "$TOOLS"

echo
echo "=== Run the timeout test ==="
cargo test -p kod-tools execute_command_times_out_without_panic 2>&1 | tail -15

echo
echo "=== Full kod-tools test run ==="
cargo test -p kod-tools 2>&1 | tail -12

echo
echo "=== Clippy ==="
cargo clippy -p kod-tools --all-targets -- -D warnings 2>&1 | tail -8

git add -A
git commit -F - <<'MSG'
fix(tools): execute_command no longer panics on timeout

The select! loop in ExecuteCommandTool::execute gated all three
arms on `!timed_out`:

  tokio::select! {
      r = &mut stdout_fut, if stdout_res.is_none() && !timed_out => { ... }
      r = &mut stderr_fut, if stderr_res.is_none() && !timed_out => { ... }
      _ = &mut timeout,    if !timed_out                       => { ... }
  }

When the timeout arm fired (set timed_out = true, killed the child)
and the loop came around to a fresh select!, every arm's guard was
false. tokio::select! with all arms disabled and no else branch
panics with "all branches are disabled and there is no else
branch".

So a command that produces no output and never exits — the exact
case the timeout exists for, e.g. `sleep 9999` — did not time out
cleanly; it panicked after the kill. The runaway-output test did
not catch it: `yes` produces output, so a read arm always completed
before the timeout had a chance to fire.

Remove `&& !timed_out` from the two read-arm guards. The reads are
polled until they finish; the child is dead by then, its death
closes the pipe write ends, and each read returns whatever bytes
were buffered followed by EOF. The timeout arm keeps `!timed_out`
so the sleep fires exactly once.

Adds execute_command_times_out_without_panic: 1-second timeout,
`sleep 60`, asserts the call returns within a few seconds and
reports timed_out: true with empty stdout. The panic this replaces
fails the test before any assertion; a missed timeout blows past
the elapsed bound.
MSG
