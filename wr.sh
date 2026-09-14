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
TARGET=crates/kod-swarm/src/communication.rs

if [ ! -f Cargo.toml ] || [ ! -f "$TARGET" ]; then
    echo "ERROR: run from the kod workspace root ($TARGET missing)"
    exit 1
fi

echo "Patching $TARGET: fix MessageContent import in broadcast tests"

python3 - "$TARGET" << 'PYEOF'
import os
import sys

target = sys.argv[1]
with open(target, "r") as f:
    content = f.read()

# `MessageContent` is a local type alias in this module
# (`pub type MessageContent = AgentMessageContent;`), not a kod-types
# export. Use `super::MessageContent` from the tests module.
old = "    use kod_types::MessageContent;"
new = "    use super::MessageContent;"

n = content.count(old)
if n != 1:
    print(f"ERROR: expected 1 occurrence, found {n}")
    sys.exit(2)
content = content.replace(old, new, 1)

tmp = target + ".tmp"
with open(tmp, "w") as f:
    f.write(content)
os.replace(tmp, target)
print("Patched", target)
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

echo "Running clippy with -D warnings"
if ! cargo clippy --workspace --all-targets -- -D warnings 2>&1; then
    echo "Clippy failed. Paste the full output for a surgical fix."
    exit 1
fi

echo "Running kod-swarm tests (120s wall clock)"
if ! run_with_timeout 120 cargo test -p kod-swarm 2>&1; then
    echo "kod-swarm tests failed or hung. Paste the full output for a surgical fix."
    exit 1
fi

echo "Running workspace tests (300s wall clock)"
if ! run_with_timeout 300 cargo test --workspace 2>&1; then
    echo "Workspace tests failed or hung. Paste the full output for a surgical fix."
    exit 1
fi

echo "All checks passed. Committing."
git add -A
git commit -m "test(swarm): import MessageContent from the local alias

The broadcast tests used 'kod_types::MessageContent', but that type
is a local alias in this module (pub type MessageContent =
AgentMessageContent;). Fix the import to 'super::MessageContent'."
