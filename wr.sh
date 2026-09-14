#!/usr/bin/env bash
set -uo pipefail

TARGET=crates/kod-swarm/src/communication.rs

if [ ! -f Cargo.toml ] || [ ! -f "$TARGET" ]; then
    echo "ERROR: run from the kod workspace root"
    exit 1
fi

echo "=== Diagnostic ==="
sed -n '436,444p' "$TARGET"

echo
echo "Removing the unused mut"

python3 - "$TARGET" << 'PYEOF'
import os
import sys

target = sys.argv[1]
with open(target, "r") as f:
    src = f.read()

old = "        let mut rx_b = hub.get_agent_receiver(&b).await.unwrap();"
new = "        let rx_b = hub.get_agent_receiver(&b).await.unwrap();"

n = src.count(old)
if n == 0:
    # Already fixed?
    if "let rx_b = hub.get_agent_receiver(&b).await.unwrap();" in src:
        print("Already fixed; nothing to do.")
        sys.exit(0)
    print("ERROR: anchor not found")
    sys.exit(2)
src = src.replace(old, new, n)
print(f"Removed `mut` from {n} binding(s)")

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
perf(provider): reuse HTTP client on model switch; drop stray mut

Two changes.

1. OpenAICompatProvider::with_model called with_api_key, which built
   a fresh reqwest::Client. The client is bound to base_url and
   api_key — neither changes on a model switch — so the previous
   code discarded a warm connection pool and TLS session cache on
   every `/model`. with_model now builds the new inner
   OpenAICompatible (the struct that holds the model) and moves the
   base_url, api_key, and client from the old provider into the new
   one. reqwest::Client is Clone (Arc internally), so the client
   move is an atomic increment.

   No public API change. The client field doc and with_model doc
   state the invariant: the client is tied to endpoint +
   credentials, not the model.

2. Remove the `mut` on the receiver binding in the earlier
   test_history_is_capped_per_agent. AgentMessageReceiver::recv
   takes &self, so the binding was never mutated — clippy -D
   warnings rejects it.
MSG

git add -A
git commit -F /tmp/kod_commit_msg.txt
rm -f /tmp/kod_commit_msg.txt
