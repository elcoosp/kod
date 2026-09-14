#!/usr/bin/env bash
set -uo pipefail

COMPILE_OK=true
INCOMPLETE=false
TARGET=crates/kod-provider-openai/src/provider.rs

if [ ! -f Cargo.toml ] || [ ! -f "$TARGET" ]; then
    echo "ERROR: run from the kod workspace root ($TARGET missing)"
    exit 1
fi

echo "Patching $TARGET: reuse one reqwest::Client across list_models calls"

python3 - "$TARGET" << 'PYEOF'
import os
import sys

target = sys.argv[1]
with open(target, "r") as f:
    content = f.read()

def patch(old, new, label, expect=1):
    global content
    n = content.count(old)
    if n == 0:
        print(f"ERROR: old snippet not found: {label}")
        sys.exit(2)
    if expect and n != expect:
        print(f"ERROR: expected {expect} occurrence(s) of {label}, found {n}")
        sys.exit(2)
    content = content.replace(old, new, expect if expect else n)
    print(f"Patched: {label}")

# --- 1. Add `client` field ------------------------------------------------
patch(
    '''/// Adapter that implements kod's [`LlmProvider`] for any OpenAI-compatible endpoint.
pub struct OpenAICompatProvider {
    inner: OpenAICompatible,
    model: String,
    base_url: String,
    api_key: String,
}''',
    '''/// Adapter that implements kod's [`LlmProvider`] for any OpenAI-compatible endpoint.
pub struct OpenAICompatProvider {
    inner: OpenAICompatible,
    model: String,
    base_url: String,
    api_key: String,
    /// Shared HTTP client for the small set of requests kod issues
    /// directly (currently `GET /v1/models`). Built once per provider so
    /// the connection pool, TLS session cache, and background runtime
    /// are reused across calls. The previous code constructed a fresh
    /// `reqwest::Client` on every `list_models()` — each one spins up
    /// its own pool and a background task, all of which are dropped as
    /// soon as the response lands.
    client: reqwest::Client,
}''',
    "client field",
)

# --- 2. Construct it in with_api_key -------------------------------------
patch(
    '''        let inner = OpenAICompatible::new(
            OpenAICompatibleConfig::new(&api_key, &model)
                .with_base_url(&base_url)
                .with_provider_name("openai-compatible"),
        )
        .map_err(adk_err)?;
        Ok(Self {
            inner,
            model,
            base_url,
            api_key,
        })
    }''',
    '''        let inner = OpenAICompatible::new(
            OpenAICompatibleConfig::new(&api_key, &model)
                .with_base_url(&base_url)
                .with_provider_name("openai-compatible"),
        )
        .map_err(adk_err)?;
        // One client per provider. A rustls-backed reqwest client
        // carries a connection pool and a TLS session cache that are
        // worth keeping warm; the pool is also what makes back-to-back
        // `/model` switches cheap.
        let client = reqwest::Client::builder()
            .build()
            .map_err(|e| KodError::Provider(format!("could not build http client: {e}")))?;
        Ok(Self {
            inner,
            model,
            base_url,
            api_key,
            client,
        })
    }''',
    "client construction",
)

# --- 3. Use the field in list_models ------------------------------------
patch(
    '''    async fn list_models(&self) -> Result<Vec<String>> {
        let response = reqwest::Client::new()
            .get(format!("{}/models", self.base_url))
            .bearer_auth(&self.api_key)
            .send()
            .await
            .map_err(|e| KodError::Provider(format!("list models request failed: {e}")))?;''',
    '''    async fn list_models(&self) -> Result<Vec<String>> {
        let response = self
            .client
            .get(format!("{}/models", self.base_url))
            .bearer_auth(&self.api_key)
            .send()
            .await
            .map_err(|e| KodError::Provider(format!("list models request failed: {e}")))?;''',
    "list_models reuses client",
)

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

echo "Running provider tests"
if ! cargo test -p kod-provider-openai 2>&1; then
    echo "provider tests failed. Paste the full output for a surgical fix."
    exit 1
fi

echo "Running workspace tests"
if ! cargo test --workspace 2>&1; then
    echo "Workspace tests failed. Paste the full output for a surgical fix."
    exit 1
fi

echo "Running clippy with -D warnings"
if ! cargo clippy --workspace --all-targets -- -D warnings 2>&1; then
    echo "Clippy failed. Paste the full output for a surgical fix."
    exit 1
fi

echo "All checks passed. Committing."
git add -A
git commit -m "perf(provider): reuse one reqwest::Client across list_models calls

OpenAICompatProvider::list_models built a fresh reqwest::Client on
every call:

    reqwest::Client::new()
        .get(format!(\"{}/models\", self.base_url))
        ...

Each Client::new spins up a connection pool, a TLS session cache,
and a background runtime task. All of that is dropped as soon as
the response lands, so every /model completion in the TUI and every
list_models call elsewhere paid the full setup cost and left the
OS-level resources to be reclaimed. It also meant back-to-back
calls (a /model autocomplete burst, a startup probe right after a
switch) could not reuse a warm TCP/TLS connection.

Add a `client: reqwest::Client` field, built once in
with_api_key via Client::builder().build(). list_models now uses
self.client.

No behavior change for callers; the struct gains one field. The
adk-backed generate/stream paths already go through
OpenAICompatible and are unaffected."
