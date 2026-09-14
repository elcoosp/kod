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
CARGO=crates/kod-config/Cargo.toml

if [ ! -f Cargo.toml ] || [ ! -f "$CARGO" ]; then
    echo "ERROR: run from the kod workspace root ($CARGO missing)"
    exit 1
fi

echo "Adding tracing dep to kod-config (used by load_default's warn! calls)"

python3 - "$CARGO" << 'PYEOF'
import os
import sys

cargo = sys.argv[1]
with open(cargo, "r") as f:
    content = f.read()

old = '''[dependencies]
serde = { workspace = true }
toml = { workspace = true }
dirs = { workspace = true }
kod-error = { path = "../kod-error" }'''

new = '''[dependencies]
serde = { workspace = true }
toml = { workspace = true }
dirs = { workspace = true }
tracing = { workspace = true }
kod-error = { path = "../kod-error" }'''

n = content.count(old)
if n != 1:
    print(f"ERROR: expected 1 occurrence of [dependencies] block, found {n}")
    sys.exit(2)
content = content.replace(old, new, 1)

tmp = cargo + ".tmp"
with open(tmp, "w") as f:
    f.write(content)
os.replace(tmp, cargo)
print("Patched", cargo)
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

echo "Running kod-config tests"
if ! run_with_timeout 60 cargo test -p kod-config 2>&1; then
    echo "kod-config tests failed or hung. Paste the full output for a surgical fix."
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
git commit -m "fix(config): add tracing dependency for load_default warnings

load_default's fallback branches use tracing::warn!, but kod-config
did not depend on tracing. Add tracing = { workspace = true } to the
crate's [dependencies]. tracing is already a workspace dep used by
every crate above kod-config, so this adds no new transitive cost."
