#!/usr/bin/env bash
set -uo pipefail

COMPILE_OK=true
TARGET=crates/kod-swarm/src/agent.rs

if [ ! -f Cargo.toml ] || [ ! -f "$TARGET" ]; then
    echo "ERROR: run from the kod workspace root ($TARGET missing)"
    exit 1
fi

echo "Exposing Agent::model_config() so the field is not dead"

python3 - "$TARGET" << 'PYEOF'
import os
import sys

target = sys.argv[1]
with open(target, "r") as f:
    content = f.read()

old = '''    /// Get model name.
    ///
    /// Returns the resolved name, which is what `AgentBuilder::with_model`
    /// overrides. The previous implementation read
    /// `model_config.model_name`, i.e. the builder's ModelConfig
    /// default, so `Agent::new("x").with_model("qwen").build().model()`
    /// returned the *default* ("codellama:13b") instead of "qwen" —
    /// the override was stored on the `model` field but never read.
    pub fn model(&self) -> &str {
        &self.model
    }'''

new = '''    /// Get model name.
    ///
    /// Returns the resolved name, which is what `AgentBuilder::with_model`
    /// overrides. The previous implementation read
    /// `model_config.model_name`, i.e. the builder's ModelConfig
    /// default, so `Agent::new("x").with_model("qwen").build().model()`
    /// returned the *default* ("codellama:13b") instead of "qwen" —
    /// the override was stored on the `model` field but never read.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// The full model configuration this agent was built with —
    /// provider, temperature, max_tokens, and the config-file model
    /// name. `model()` returns the resolved name (which
    /// `with_model` can override); this accessor exposes the rest.
    /// Used by status panels and by callers that need to clone an
    /// agent's settings.
    pub fn model_config(&self) -> &ModelConfig {
        &self.model_config
    }'''

n = content.count(old)
if n != 1:
    print(f"ERROR: expected 1 occurrence of Agent::model, found {n}")
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

echo "cargo clippy --workspace --all-targets -- -D warnings"
if ! cargo clippy --workspace --all-targets -- -D warnings 2>&1; then
    echo "Clippy failed"
    exit 1
fi

echo "Committing."
git add -A
git commit -m "fix(swarm): expose Agent::model_config() to un-dead the field

After Agent::model() was corrected to return the resolved \`model\`
field, the \`model_config\` field became unread and clippy -D
warnings rejected it as dead_code.

Rather than \\#[allow(dead_code)], expose the field via a new
\`model_config(&self) -> &ModelConfig\` accessor. The struct's
provider, temperature, and max_tokens are legitimate for a status
panel or for cloning an agent's configuration — \`model()\` only
returns the resolved name."
