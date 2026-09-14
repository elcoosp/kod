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
APP=crates/kod-tui/src/app.rs
LOOP=crates/kod-tui/src/main_loop.rs

for f in "$APP" "$LOOP"; do
    if [ ! -f "$f" ]; then
        echo "ERROR: missing $f — run from the kod workspace root"
        exit 1
    fi
done

echo "Fixing 5 clippy lints in kod-tui (app.rs x3, main_loop.rs x2)"

python3 - "$APP" "$LOOP" << 'PYEOF'
import os
import sys

app, loop = sys.argv[1], sys.argv[2]

def patch(path, old, new, label, expect=1):
    with open(path, "r") as f:
        content = f.read()
    n = content.count(old)
    if n == 0:
        print(f"ERROR: old snippet not found in {path}: {label}")
        sys.exit(2)
    if expect and n != expect:
        print(f"ERROR: expected {expect} occurrence(s) of {label} in {path}, found {n}")
        sys.exit(2)
    patched = content.replace(old, new, expect if expect else n)
    tmp = path + ".tmp"
    with open(tmp, "w") as f:
        f.write(patched)
    os.replace(tmp, path)
    print(f"Patched {path}: {label}")

# --- 1. app.rs: if_same_then_else in friendly_error ---------------------
# The `else if lower.contains("cancelled by user")` arm and the final
# `else` both return "". Collapse: drop the redundant arm.
patch(
    app,
    '''        } else if lower.contains("timed out")
            || lower.contains("timeout")
            || lower.contains("deadline")
        {
            " The request timed out — the model may still be loading (first run pulls weights). Wait a minute and `/retry`."
        } else if lower.contains("cancelled by user") {
            ""
        } else {
            ""
        };''',
    '''        } else if lower.contains("timed out")
            || lower.contains("timeout")
            || lower.contains("deadline")
        {
            " The request timed out — the model may still be loading (first run pulls weights). Wait a minute and `/retry`."
        } else {
            // Any other error carries no situational advice. This also
            // covers "cancelled by user" (which the engine treats as a
            // normal stop, not a failure) and matches the previous
            // behavior of returning an empty advice string for both.
            ""
        };''',
    "collapse identical if-same-then-else arms in friendly_error",
)

# --- 2. app.rs: unnecessary unwrap in spinner() -------------------------
# `if self.generating && self.spinner_started.is_some()` then
# `self.spinner_started.unwrap()`. Use if-let to avoid the unwrap.
patch(
    app,
    '''    pub fn spinner(&self) -> &'static str {
        if self.generating && self.spinner_started.is_some() {
            let started = self.spinner_started.unwrap();
            let step = started.elapsed().as_millis() / 100;
            return SPINNER_FRAMES[(step as usize) % SPINNER_FRAMES.len()];
        }
        SPINNER_FRAMES[self.spinner_frame % SPINNER_FRAMES.len()]
    }''',
    '''    pub fn spinner(&self) -> &'static str {
        if self.generating
            && let Some(started) = self.spinner_started
        {
            let step = started.elapsed().as_millis() / 100;
            return SPINNER_FRAMES[(step as usize) % SPINNER_FRAMES.len()];
        }
        SPINNER_FRAMES[self.spinner_frame % SPINNER_FRAMES.len()]
    }''',
    "if-let on spinner_started",
)

# --- 3. app.rs: collapsible_if in expand_tilde --------------------------
# `else if raw == "~" { if let Some(home) = dirs::home_dir() { … } }`
# collapses into a let-chain on edition 2024.
patch(
    app,
    '''    fn expand_tilde(raw: &str) -> String {
        if let Some(rest) = raw.strip_prefix("~/") {
            if let Some(home) = dirs::home_dir() {
                return format!("{}/{rest}", home.display());
            }
        } else if raw == "~" {
            if let Some(home) = dirs::home_dir() {
                return home.display().to_string();
            }
        }
        raw.to_string()
    }''',
    '''    fn expand_tilde(raw: &str) -> String {
        if let Some(rest) = raw.strip_prefix("~/") {
            if let Some(home) = dirs::home_dir() {
                return format!("{}/{rest}", home.display());
            }
        } else if raw == "~"
            && let Some(home) = dirs::home_dir()
        {
            return home.display().to_string();
        }
        raw.to_string()
    }''',
    "collapse collapsible_if in expand_tilde",
)

# --- 4. main_loop.rs: map_identity on the panic-hook restore ------------
patch(
    loop,
    '''        let default_hook = std::sync::Arc::try_unwrap(default_hook)
            .map(|h| h)
            .unwrap_or_else(|_| std::panic::take_hook());''',
    '''        let default_hook = std::sync::Arc::try_unwrap(default_hook)
            .unwrap_or_else(|_| std::panic::take_hook());''',
    "remove map_identity",
)

# --- 5. main_loop.rs: redundant_pattern_matching on pending_confirm -----
patch(
    loop,
    '''        if let Some(_) = self.app.pending_confirm() {''',
    '''        if self.app.pending_confirm().is_some() {''',
    "is_some() in place of if-let Some(_)",
)

print("All patches applied.")
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
    echo "Clippy still failing. Paste the full output for a surgical fix."
    exit 1
fi

echo "Running kod-tui tests (120s wall clock)"
if ! run_with_timeout 120 cargo test -p kod-tui 2>&1; then
    echo "kod-tui tests failed or hung. Paste the full output for a surgical fix."
    exit 1
fi

echo "Running workspace tests (300s wall clock)"
if ! run_with_timeout 300 cargo test --workspace 2>&1; then
    echo "Workspace tests failed or hung. Paste the full output for a surgical fix."
    exit 1
fi

echo "All checks passed. Committing."
git add -A
git commit -m "style(tui): clear five clippy lints blocking -D warnings

kod-tui now compiles clean under clippy -D warnings:

- app.rs friendly_error: the 'cancelled by user' arm returned the
  same empty string as the final else. Drop the redundant arm and
  note in the remaining else that cancelled-by-user is a normal
  stop, not a failure.

- app.rs spinner: 'if generating && started.is_some()' followed by
  started.unwrap(). Rewrite as 'if generating && let Some(started)'.

- app.rs expand_tilde: nested if-let under 'else if raw == \"~\"'.
  Collapse into a single let-chain (edition 2024).

- main_loop.rs panic-hook restore: '.map(|h| h)' is the identity.
  Remove it; Arc::try_unwrap's Ok arm already yields the inner value.

- main_loop.rs pending_confirm: 'if let Some(_) = ...' rewritten as
  '.is_some()'.

No behavior change."
