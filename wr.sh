#!/usr/bin/env bash
set -uo pipefail

run_with_timeout() {
    local secs="$1"; shift
    if command -v timeout >/dev/null 2>&1; then
        timeout "$secs" "$@"; return $?
    fi
    if command -v gtimeout >/dev/null 2>&1; then
        gtimeout "$secs" "$@"; return $?
    fi
    "$@" &
    local pid=$!
    ( sleep "$secs"
      if kill -0 "$pid" 2>/dev/null; then
          kill -TERM "$pid" 2>/dev/null
          sleep 2
          kill -KILL "$pid" 2>/dev/null
      fi ) &
    local watchdog=$!
    wait "$pid"; local rc=$?
    kill "$watchdog" 2>/dev/null; wait "$watchdog" 2>/dev/null
    [ "$rc" -ge 128 ] && return 124
    return "$rc"
}

COMPILE_OK=true
INCOMPLETE=false
TARGET=crates/kod-tui/tests/ui.rs

if [ ! -f Cargo.toml ] || [ ! -f "$TARGET" ]; then
    echo "ERROR: run from the kod workspace root ($TARGET missing)"
    exit 1
fi

echo "Patching $TARGET: derive expected completion count from SLASH_COMMANDS"

python3 - "$TARGET" << 'PYEOF'
import os
import re
import sys

target = sys.argv[1]
with open(target, "r") as f:
    lines = f.readlines()

# Locate the test function `test_slash_completion_filter_and_accept`.
# Find its start line, then walk forward tracking brace depth to find
# the matching close. We only edit text inside that span.
func_re = re.compile(r"^\s*fn\s+test_slash_completion_filter_and_accept\s*\(")
start = None
for i, line in enumerate(lines):
    if func_re.search(line):
        start = i
        break
if start is None:
    print("ERROR: could not find test_slash_completion_filter_and_accept")
    sys.exit(2)

# Brace-count from the start line onward. The signature line contains
# '{' typically; if not, we find it on a later line.
depth = 0
end = None
seen_open = False
for j in range(start, len(lines)):
    for ch in lines[j]:
        if ch == '{':
            depth += 1
            seen_open = True
        elif ch == '}':
            depth -= 1
            if seen_open and depth == 0:
                end = j
                break
    if end is not None:
        break
if end is None:
    print("ERROR: could not find end of test function")
    sys.exit(2)

print(f"Test spans lines {start + 1}..{end + 1}")

# Within [start, end], replace assertions that hardcode 16 (the old
# SLASH_COMMANDS length) with a derivation from SLASH_COMMANDS.len().
#
# Two shapes observed in this workspace's test style:
#   assert_eq!(SOMETHING.len(), 16);          ->  SLASH_COMMANDS.len()
#   assert_eq!(16, SOMETHING.len());          ->  SLASH_COMMANDS.len()
#   assert_eq!(app.completion_candidates().len(), 16);
# Also cover a bare `let n = 16;` if it exists.
replaced = 0
for i in range(start, end + 1):
    line = lines[i]
    new_line = line
    # Pattern A: ", 16)" at end of an assert_eq! argument list
    if re.search(r",\s*16\s*\)", line) and (
        "SLASH_COMMANDS" in line
        or "completion_candidates" in line
        or "candidates" in line
    ):
        new_line = re.sub(r",\s*16\s*\)", ", SLASH_COMMANDS.len())", new_line)
    # Pattern B: "(16," leading
    elif re.search(r"\(\s*16\s*,", line) and (
        "SLASH_COMMANDS" in line
        or "completion_candidates" in line
        or "candidates" in line
    ):
        new_line = re.sub(r"\(\s*16\s*,", "(SLASH_COMMANDS.len(),", new_line)
    # Pattern C: "== 16" or "!= 16"
    elif re.search(r"[!=]=\s*16\b", line) and (
        "SLASH_COMMANDS" in line
        or "completion_candidates" in line
        or "candidates" in line
    ):
        new_line = re.sub(r"([!=]=\s*)16\b", r"\1SLASH_COMMANDS.len()", new_line)
    if new_line != line:
        print(f"  line {i + 1}: {line.rstrip()!r} -> {new_line.rstrip()!r}")
        lines[i] = new_line
        replaced += 1

if replaced == 0:
    print("ERROR: no hardcoded-16 assertion found inside the test.")
    print("Full function body follows for diagnosis:")
    for i in range(start, end + 1):
        print(f"  {i + 1}: {lines[i].rstrip()}")
    sys.exit(2)

# Make sure SLASH_COMMANDS is imported in the test file. If the test
# uses `use kod_tui::app::{...}`, extend it; if it uses the re-export
# `use kod_tui::{...}`, extend that.
content = "".join(lines)
if "SLASH_COMMANDS" not in content.split("fn test_slash_completion_filter_and_accept", 1)[0]:
    # Not yet imported anywhere above the test. Try to extend a
    # plausible existing import.
    if "use kod_tui::app::{" in content:
        content = content.replace(
            "use kod_tui::app::{",
            "use kod_tui::app::{\n    SLASH_COMMANDS,",
            1,
        )
        print("Added SLASH_COMMANDS to `use kod_tui::app::{...}`")
    elif "use kod_tui::{KodApp" in content:
        content = content.replace(
            "use kod_tui::{KodApp",
            "use kod_tui::{KodApp, SLASH_COMMANDS",
            1,
        )
        print("Added SLASH_COMMANDS to `use kod_tui::{...}`")
    else:
        print("ERROR: could not find an import line to extend with SLASH_COMMANDS")
        sys.exit(2)
    lines = content.splitlines(keepends=True)

tmp = target + ".tmp"
with open(tmp, "w") as f:
    f.writelines(lines)
os.replace(tmp, target)
print(f"Patched {target}: replaced {replaced} hardcoded count(s)")
PYEOF

if [ $? -ne 0 ]; then
    echo "ERROR: patching failed"
    exit 1
fi

echo "Checking compilation"
if ! cargo check --workspace 2>&1; then
    echo "Compilation failed – will skip commit"
    COMPILE_OK=false
fi

if [ "$INCOMPLETE" = true ] || [ "$COMPILE_OK" = false ]; then
    echo "Skipping tests and commit due to incomplete files or compilation errors"
    exit 1
fi

echo "Running kod-tui tests (180s wall clock)"
if ! run_with_timeout 180 cargo test -p kod-tui 2>&1; then
    echo "kod-tui tests failed or hung. Paste the full output for a surgical fix."
    exit 1
fi

echo "Running workspace tests (300s wall clock)"
if ! run_with_timeout 300 cargo test --workspace 2>&1; then
    echo "Workspace tests failed or hung. Paste the full output for a surgical fix."
    exit 1
fi

echo "Running clippy with -D warnings"
if ! cargo clippy --workspace --all-targets -- -D warnings 2>&1; then
    echo "Clippy failed. Paste the full output for a surgical fix."
    exit 1
fi

echo "All checks passed. Committing."
git add -A
git commit -m "test(tui): derive completion count from SLASH_COMMANDS

test_slash_completion_filter_and_accept asserted the total was the
literal 16. That was correct until /debug was added to
SLASH_COMMANDS, at which point the count became 17 and the test
failed with '17 == 16' — a brittleness with no purpose, since the
test's actual intent is 'the popup lists every command'.

Replace the hardcoded number with SLASH_COMMANDS.len(), so adding,
removing, or reordering a command cannot break this test again.
Same for any other assert_eq! in this test that referenced the
literal count."
