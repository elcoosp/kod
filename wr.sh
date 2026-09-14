#!/usr/bin/env bash
set -uo pipefail

COMPILE_OK=true
LLM=crates/kod-config/src/llm.rs

if [ ! -f Cargo.toml ] || [ ! -f "$LLM" ]; then
    echo "ERROR: run from the kod workspace root ($LLM missing)"
    exit 1
fi

echo "Patching $LLM: collapse redundant OpenAI variant; warn on unsupported providers"

python3 - "$LLM" << 'PYEOF'
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

# --- 1. Collapse OpenAI into OpenAICompatible as an alias --------------
patch(
    '''#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ProviderType {
    /// Any OpenAI-spec chat-completions endpoint (Ollama `/v1`, LM Studio,
    /// MLX Omni Serve, vLLM, OpenAI). `Ollama` is kept as a deprecated alias
    /// so existing config files keep loading.
    #[serde(alias = "Ollama")]
    OpenAICompatible,
    Anthropic,
    OpenAI,
    Custom,
}''',
    '''#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ProviderType {
    /// Any OpenAI-spec chat-completions endpoint: Ollama `/v1`, LM Studio,
    /// MLX Omni Serve, vLLM, and OpenAI itself — they all speak the same
    /// wire protocol, so one code path serves them all. `Ollama` and
    /// `OpenAI` are accepted as aliases so config files written against
    /// earlier enum names keep loading.
    #[serde(alias = "Ollama", alias = "OpenAI")]
    OpenAICompatible,
    /// Anthropic's Messages API. Not implemented — a config with this
    /// provider loads (so `kod config` shows it) but the first prompt
    /// will fail. `LlmConfig::validate` warns about this at startup.
    Anthropic,
    /// Anything else. Same situation as Anthropic: recognised as a
    /// provider name, not implemented.
    Custom,
}''',
    "collapse OpenAI into OpenAICompatible",
)

# --- 2. Warn on non-OpenAICompatible providers in validate --------------
patch(
    '''        // Model: empty model name is rejected by every provider.
        if self.model.trim().is_empty() {
            tracing::warn!(
                "llm.model is empty; falling back to {}",
                LlmConfig::default().model
            );
            self.model = LlmConfig::default().model;
        }
    }''',
    '''        // Model: empty model name is rejected by every provider.
        if self.model.trim().is_empty() {
            tracing::warn!(
                "llm.model is empty; falling back to {}",
                LlmConfig::default().model
            );
            self.model = LlmConfig::default().model;
        }

        // Provider: only OpenAICompatible is implemented today. The
        // other variants are recognised so a config with them loads
        // (the user can still see it via `kod config`), but the CLI
        // constructs an OpenAI-compatible client regardless, so an
        // Anthropic or Custom config fails at the first prompt with
        // whatever error the server returns. Warn loudly here so the
        // mismatch is named at startup rather than discovered after a
        // wasted prompt.
        match self.provider {
            ProviderType::OpenAICompatible => {}
            ProviderType::Anthropic | ProviderType::Custom => {
                tracing::warn!(
                    provider = ?self.provider,
                    "llm.provider names a protocol that kod does not yet speak; \\
                     the CLI will send OpenAI-compatible requests to {} and the \\
                     server is likely to reject them. Set provider = \\"OpenAICompatible\\" \\
                     for now (Anthropic and Custom support is planned).",
                    self.base_url
                );
            }
        }
    }''',
    "provider validation warning",
)

# --- 3. Test that OpenAI is accepted as an alias and validate warns ----
patch(
    '''    #[test]
    fn test_legacy_ollama_provider_alias() {''',
    '''    /// `provider = "OpenAI"` must still deserialize — the OpenAI API
    /// IS the OpenAI-compatible protocol, so the two variants were
    /// merged. A config written against the old enum value must keep
    /// loading.
    #[test]
    fn test_legacy_openai_provider_alias() {
        let config: LlmConfig = toml::from_str(
            r#"
            provider = "OpenAI"
            model = "gpt-4o-mini"
            base_url = "https://api.openai.com"
            context_window = 128000
            max_tokens = 4096
            temperature = 0.7
            timeout_secs = 120
            "#,
        )
        .unwrap();
        assert_eq!(config.provider, ProviderType::OpenAICompatible);
    }

    /// Non-OpenAICompatible providers must not silently pass through
    /// validate(); the warn! cannot be asserted directly, but the
    /// variant must survive validate() unchanged (no accidental
    /// normalization), and OpenAICompatible must be a no-op.
    #[test]
    fn test_validate_leaves_provider_choice_intact() {
        // Anthropic is unsupported but loadable. validate must not
        // rewrite it — the warning is the entire user-facing signal,
        // and changing the enum behind the user's back would be worse
        // than the warning.
        let mut c = LlmConfig {
            provider: ProviderType::Anthropic,
            ..LlmConfig::default()
        };
        c.validate();
        assert_eq!(c.provider, ProviderType::Anthropic);

        let mut c = LlmConfig {
            provider: ProviderType::Custom,
            ..LlmConfig::default()
        };
        c.validate();
        assert_eq!(c.provider, ProviderType::Custom);

        let mut c = LlmConfig::default();
        c.validate();
        assert_eq!(c.provider, ProviderType::OpenAICompatible);
    }

    #[test]
    fn test_legacy_ollama_provider_alias() {''',
    "OpenAI alias + provider tests",
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

echo "cargo check --workspace --all-targets"
if ! cargo check --workspace --all-targets 2>&1; then
    echo "Compilation failed"
    exit 1
fi

echo "cargo clippy --workspace --all-targets -- -D warnings"
if ! cargo clippy --workspace --all-targets -- -D warnings 2>&1; then
    echo "Clippy failed"
    exit 1
fi

echo "Committing."
git add -A
git commit -m "fix(config): collapse redundant OpenAI variant; warn on unsupported providers

ProviderType had four variants: OpenAICompatible, Anthropic,
OpenAI, and Custom. But OpenAI's chat-completions API *is* the
OpenAI-compatible protocol — the two are the same wire format, so
carrying both variants was redundant. A config with
\\`provider = \"OpenAI\"\\` worked only by accident: the CLI constructs
an OpenAICompatProvider regardless, so the variant was accepted but
never consulted for anything the OpenAICompatible one would not
have done.

Merge OpenAI into OpenAICompatible as a serde alias. Existing
configs keep loading; the enum loses a synonym that only invited
the reader to look for a difference that did not exist.

Anthropic and Custom stay as recognised-but-unimplemented variants
— removing them would break configs that name them, and the docs
already list Anthropic as planned. LlmConfig::validate now warns at
startup when either is set, naming the consequence ('the CLI will
send OpenAI-compatible requests to <base_url> and the server is
likely to reject them') and the fix (set OpenAICompatible for now).
Before this, an Anthropic config produced an opaque HTTP error on
the first prompt; the warning makes the mismatch visible at load.

Adds two tests: test_legacy_openai_provider_alias pins the alias,
and test_validate_leaves_provider_choice_intact confirms validate()
does not rewrite the user's choice (the warning is the signal)."
