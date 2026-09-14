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
TARGET=.github/workflows/ci.yml

if [ ! -f Cargo.toml ] || [ ! -f "$TARGET" ]; then
    echo "ERROR: run from the kod workspace root ($TARGET missing)"
    exit 1
fi

echo "Patching $TARGET: add a live-model job against a real Ollama server"

python3 - "$TARGET" << 'PYEOF'
import os
import sys

target = sys.argv[1]
with open(target, "r") as f:
    content = f.read()

old = '''  benchmark:
    name: Benchmarks
    runs-on: ubuntu-latest
    if: github.event_name == 'push'
'''

new = '''  # Runs the #[ignore]-gated tests that exercise the product against a
  # real OpenAI-compatible server. Without this job, the ignored tests
  # never execute anywhere and a regression in the streaming loop,
  # tool-call assembly, or list_models ships without a signal.
  #
  # Uses the smallest reliable Ollama model (qwen2.5:0.5b, ~350 MB) so
  # pull time stays under a minute on a warm cache. The Ollama setup is
  # the standard installer rather than a marketplace action: the
  # installer is stable, and the actions in that space have had flaky
  # releases.
  live-model:
    name: Live model (Ollama)
    runs-on: ubuntu-latest
    # Skip on PRs from forks that can't install services; run on every
    # push to main/develop and on PRs from the same repo.
    if: github.event_name == 'push' || github.event.pull_request.head.repo.full_name == github.repository

    steps:
    - uses: actions/checkout@v4

    - name: Install Rust
      uses: dtolnay/rust-toolchain@stable

    - name: Cache cargo registry
      uses: actions/cache@v4
      with:
        path: |
          ~/.cargo/registry
          ~/.cargo/git
          target
        key: ${{ runner.os }}-cargo-live-${{ hashFiles('**/Cargo.lock') }}
        restore-keys: |
          ${{ runner.os }}-cargo-live-

    - name: Install Ollama
      run: |
        curl -fsSL https://ollama.com/install.sh | sh
        ollama --version

    - name: Start Ollama
      run: |
        # The systemd unit the installer ships does not run in the
        # Actions sandbox, so start the server directly and wait for
        # /api/tags to answer.
        nohup ollama serve > /tmp/ollama.log 2>&1 &
        for i in $(seq 1 30); do
          if curl -sf http://localhost:11434/api/tags > /dev/null; then
            echo "Ollama is up after ${i}s"
            exit 0
          fi
          sleep 1
        done
        echo "Ollama did not start within 30s; log follows:" >&2
        cat /tmp/ollama.log >&2
        exit 1

    - name: Pull tiny model
      run: ollama pull qwen2.5:0.5b

    - name: Run live provider tests
      env:
        KOD_TEST_MODEL: qwen2.5:0.5b
      run: cargo test -p kod-provider-openai -- --ignored --nocapture

    - name: Run live TUI round-trip test
      env:
        KOD_TEST_MODEL: qwen2.5:0.5b
        KOD_TEST_DB: /tmp/kod-test.redb
      run: |
        # The TUI's live test needs a kod config pointing at localhost.
        # Write a minimal one into the config directory the CLI reads.
        mkdir -p "$HOME/.config/kod"
        cat > "$HOME/.config/kod/config.toml" <<'CFG'
        [llm]
        provider = "OpenAICompatible"
        model = "qwen2.5:0.5b"
        base_url = "http://localhost:11434"
        context_window = 8192
        max_tokens = 512
        temperature = 0.2
        timeout_secs = 120
        CFG
        cargo test -p kod-tui --test main_loop -- --ignored --nocapture

  benchmark:
    name: Benchmarks
    runs-on: ubuntu-latest
    if: github.event_name == 'push'
'''

n = content.count(old)
if n != 1:
    print(f"ERROR: expected 1 occurrence of the benchmark job, found {n}")
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

echo "Validating YAML syntax"
python3 - << 'PYEOF'
import sys
try:
    import yaml  # type: ignore
except ImportError:
    print("PyYAML not installed; skipping strict validation.")
    sys.exit(0)
with open(".github/workflows/ci.yml") as f:
    data = yaml.safe_load(f)
jobs = data.get("jobs", {})
assert "live-model" in jobs, "live-model job missing"
assert "test" in jobs and "security" in jobs and "coverage" in jobs and "benchmark" in jobs
print("YAML parses; jobs:", ", ".join(sorted(jobs.keys())))
PYEOF

echo "Checking compilation (workflow change should not affect it, but keep the gate)"
if ! cargo check --workspace 2>&1; then
    echo "Compilation failed – will skip commit"
    COMPILE_OK=false
fi

if [ "$INCOMPLETE" = true ] || [ "$COMPILE_OK" = false ]; then
    echo "Skipping tests and commit due to incomplete files or compilation errors"
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
git commit -m "ci: run the ignored live-model tests against a real Ollama server

The provider crate's only end-to-end test (test_live_list_models) and
the TUI's only end-to-end test (test_live_prompt_roundtrip) are
#[ignore]-gated, and no CI job ran them. A regression in the OpenAI-
compatible path, the streaming loop, or the tool-call assembly could
ship without any signal.

Add a live-model job that:
- installs Ollama via the official installer,
- starts the server and waits for /api/tags,
- pulls qwen2.5:0.5b (~350 MB, under a minute on warm cache),
- runs 'cargo test -p kod-provider-openai -- --ignored' with
  KOD_TEST_MODEL set,
- writes a minimal config pointing at localhost and runs the TUI's
  ignored round-trip test.

The job runs on push to main/develop and on same-repo PRs (skips
fork PRs, which cannot install services). Uses the stable installer
rather than a marketplace action — those have been flaky."
