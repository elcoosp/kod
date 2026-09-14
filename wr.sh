#!/usr/bin/env bash
set -uo pipefail

ENGINE=crates/kod-core/src/engine.rs
LOOP=crates/kod-tui/src/main_loop.rs

for f in "$ENGINE" "$LOOP"; do
    if [ ! -f "$f" ]; then
        echo "ERROR: missing $f"
        exit 1
    fi
done

echo "=== Callers of list_models ==="
grep -rn "\.list_models()" crates/ | grep -v "/target/"

echo
echo "=== Current KodEngine::list_models ==="
awk '/pub async fn list_models/,/^    \}$/' "$ENGINE"

echo
python3 - "$ENGINE" "$LOOP" << 'PYEOF'
import os
import sys

engine, loop = sys.argv[1], sys.argv[2]

def patch(path, old, new, label, expect=1):
    with open(path) as f:
        src = f.read()
    n = src.count(old)
    if n == 0:
        print(f"  MISS in {path}: {label}")
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

# ======================================================================
# 1. KodEngine::list_models → Result<Vec<String>>.
#    "No provider" stays Ok(empty) — there really are zero models.
#    "Provider call failed" becomes Err — the answer is unknown.
# ======================================================================
patch(
    engine,
    '''    /// List available models from the provider, if one is set.
    pub async fn list_models(&self) -> Vec<String> {
        let provider = self.provider.read().await;
        if let Some(p) = provider.as_ref() {
            match p.list_models().await {
                Ok(models) => models,
                Err(e) => {
                    tracing::warn!(
                        error = ?e,
                        "list models request failed — \\
                         check provider base_url and API key"
                    );
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        }
    }''',
    '''    /// List available models from the configured provider.
    ///
    /// Returns `Ok(vec![])` when no provider is set — there really
    /// are zero models to list, and an empty list is the correct
    /// answer. Returns `Err(...)` when a provider *is* set but the
    /// request to it fails — the answer is unknown (server down, auth
    /// wrong, endpoint mistyped), and collapsing that into an empty
    /// vec makes a caller unable to distinguish "the server has no
    /// models" from "the server is not reachable." The two deserve
    /// different user-facing messages and different recovery paths.
    pub async fn list_models(&self) -> Result<Vec<String>> {
        let provider = self.provider.read().await;
        match provider.as_ref() {
            Some(p) => {
                let models = p.list_models().await?;
                Ok(models)
            }
            None => Ok(Vec::new()),
        }
    }''',
    "KodEngine::list_models -> Result",
)

# ======================================================================
# 2. TUI caller #1 (init_engine): best-effort for the completion cache.
#    Treat failure the same as empty — nothing to complete against.
# ======================================================================
patch(
    loop,
    '''        // Load model list from provider so /model tab-completion is useful.
        if let Some(engine) = &self.engine {
            let models = engine.list_models().await;
            self.app.set_available_models(models);
        }''',
    '''        // Load model list from provider so /model tab-completion is
        // useful. Best-effort: a failure here just means no
        // completion candidates, which /model (no args) will later
        // report explicitly when the user asks.
        if let Some(engine) = &self.engine {
            let models = engine.list_models().await.unwrap_or_default();
            self.app.set_available_models(models);
        }''',
    "init_engine: unwrap_or_default",
)

# ======================================================================
# 3. TUI caller #2 (show_and_refresh_models): distinguish the two
#    states in the message.
# ======================================================================
patch(
    loop,
    '''        let models = engine.list_models().await;
        if models.is_empty() {
            self.app.push_system_message(
                "No models reported by the provider. Is the server running? \\
                 For Ollama: `ollama serve`, then `/retry`.",
            );
            return Ok(());
        }''',
    '''        let models = match engine.list_models().await {
            Ok(m) => m,
            Err(e) => {
                // The provider is set but the request failed. Name the
                // failure instead of reporting an empty list — the
                // user's recovery step differs: fix the server, not
                // "there is nothing to see."
                self.app.push_system_message(&format!(
                    "Could not list models from the provider: {e}\\n\\
                     Check that the server is running and `base_url` in the \\
                     kod config is correct. For Ollama: `ollama serve`, then \\
                     `/model` again.",
                ));
                return Ok(());
            }
        };
        if models.is_empty() {
            // No error, but the server really has zero models.
            self.app.push_system_message(
                "The provider is reachable but reports no models. \\
                 Pull one first (e.g. `ollama pull qwen2.5:0.5b`), then \\
                 `/model` again.",
            );
            return Ok(());
        }''',
    "show_and_refresh_models: distinguish error from empty",
)

print("Done.")
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
echo "cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -8"
if ! cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -8; then
    echo "Clippy failed"
    exit 1
fi

cat > /tmp/kod_commit_msg.txt <<'MSG'
feat(core,tui): list_models distinguishes "no models" from "server down"

KodEngine::list_models returned Vec<String> and collapsed two
different outcomes into an empty vector:

  - no provider set (there really are zero models to list);
  - a provider set whose list_models call failed (the answer is
    unknown — the server may be down, the auth may be wrong, the
    endpoint may be mistyped).

A caller could not tell them apart, so the TUI's /model command
printed "No models reported by the provider. Is the server
running?" — hedging between the two cases in a single sentence
because it had no way to know.

Change list_models to Result<Vec<String>>. The distinction the new
signature makes:

  Ok(vec![])  — no provider set, or the server has zero models;
  Err(...)    — a provider is set and the request failed.

The two TUI callers:

  - init_engine (completion cache): unwrap_or_default — best-effort,
    no completion candidates on failure, which /model later reports.

  - show_and_refresh_models: on Err, say "Could not list models
    from the provider: <e>" and name the fix (server running,
    base_url). On Ok(empty), say "The provider is reachable but
    reports no models — pull one first." Two distinct messages
    replace the one hedged sentence.

No public API change beyond the return type; the adk-backed
generate/stream paths are unaffected.
MSG

git add -A
git commit -F /tmp/kod_commit_msg.txt
rm -f /tmp/kod_commit_msg.txt
