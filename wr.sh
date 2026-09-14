#!/usr/bin/env bash
set -uo pipefail

CLI=crates/kod-cli/src/commands.rs

if [ ! -f Cargo.toml ] || [ ! -f "$CLI" ]; then
    echo "ERROR: run from the kod workspace root ($CLI missing)"
    exit 1
fi

echo "=== Current CLI pump ==="
awk '/let pump = tokio::spawn/,/^        \}\);/' "$CLI" | head -30

echo
echo "Patching $CLI"

python3 - "$CLI" << 'PYEOF'
import os
import sys

target = sys.argv[1]
with open(target, "r") as f:
    src = f.read()

old = '''        let pump = tokio::spawn(async move {
            let mut streamed_any = false;
            while let Some(chunk) = rx.recv().await {
                if kod_core::engine::parse_tool_start(&chunk).is_some()
                    || kod_core::engine::parse_tool_args(&chunk).is_some()
                    || kod_core::engine::parse_tool_done(&chunk).is_some()
                    || kod_core::engine::is_thinking_marker(&chunk)
                {
                    continue;
                }
                print!("{}", chunk);
                let _ = io::stdout().flush();
                streamed_any = true;
            }
            streamed_any
        });'''

new = '''        let pump = tokio::spawn(async move {
            // `streamed_any` counts text chunks, not control markers.
            // It decides whether the caller still needs to print the
            // final text: if the reply was already streamed live, the
            // caller skips the duplicate. A tool notice is not a text
            // chunk — printing it must not suppress the summary.
            let mut streamed_any = false;
            while let Some(chunk) = rx.recv().await {
                // Tool-args marker: the engine has assembled a tool
                // call and knows what it is about to do. Print a
                // one-line notice so the user sees activity between
                // two stretches of streamed text rather than an
                // unexplained pause. Uses the same brief the TUI
                // shows in its running row (format_call_brief), so
                // the two surfaces speak the same vocabulary:
                // `[execute_command cargo test]`,
                // `[read_file path=src/main.rs]`.
                if let Some(brief) = kod_core::engine::parse_tool_args(&chunk) {
                    print!("\\n[{brief}]\\n");
                    let _ = io::stdout().flush();
                    continue;
                }
                // Other control markers (tool start, tool done,
                // thinking) carry information the CLI does not
                // render. Consume them without printing — the
                // preceding tool notice already covers the visible
                // activity, and the model's follow-up text will
                // arrive as ordinary streamed chunks.
                if kod_core::engine::parse_tool_start(&chunk).is_some()
                    || kod_core::engine::parse_tool_done(&chunk).is_some()
                    || kod_core::engine::is_thinking_marker(&chunk)
                {
                    continue;
                }
                print!("{chunk}");
                let _ = io::stdout().flush();
                streamed_any = true;
            }
            streamed_any
        });'''

n = src.count(old)
if n == 0:
    print("  SKIP: pump anchor not found (already patched?)")
    sys.exit(0)
if n != 1:
    print(f"  ERROR: expected 1 pump, found {n}")
    sys.exit(2)
src = src.replace(old, new, 1)

tmp = target + ".tmp"
with open(tmp, "w") as f:
    f.write(src)
os.replace(tmp, target)
print("  patched CLI pump")
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
feat(cli): show a one-line tool notice in `kod chat`

The streaming pump in run_chat consumed every \0kod-* control marker
without printing anything. A multi-round reply therefore appeared
in the terminal as text, a pause (the tool running), more text,
with no visible signal that anything happened during the pause. The
user could not tell a slow tool call from a stalled model.

Print a one-line notice when the engine emits the tool-args marker
— the point where the arguments are assembled and the tool is about
to run. Use the brief the engine already formats
(format_call_brief), which is the same string the TUI shows in its
running row:

  [execute_command cargo test]
  [read_file path=src/main.rs]

The other three markers (tool start, tool done, thinking) stay
consumed silently: the notice above already names the activity, the
follow-up text arrives as ordinary streamed chunks, and the tool
summary belongs to the model's own reply rather than a second
console line.

`streamed_any` continues to count only text chunks, not notices.
That is what decides whether the caller still prints the final text
at the end: a reply that never streamed a token (tool-only, with
the summary produced after the loop) still gets its summary
printed. A notice does not suppress that.

No public API change.
MSG

git add -A
git commit -F /tmp/kod_commit_msg.txt
rm -f /tmp/kod_commit_msg.txt
