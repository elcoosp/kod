//! LLM provider configuration.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LlmConfig {
    pub provider: ProviderType,
    pub model: String,
    pub base_url: String,
    pub api_key: Option<String>,
    pub context_window: usize,
    pub max_tokens: usize,
    pub temperature: f32,
    pub timeout_secs: u64,
    /// Allow the agent to reach the network through `web_fetch` (and,
    /// once a search tool exists, that too). Default false: the agent
    /// running with a user's shell privileges should not reach out
    /// without an explicit opt-in. A local-only session with a local
    /// model gains nothing from it; a session that does need docs at a
    /// URL turns it on.
    pub network_access: bool,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            provider: ProviderType::OpenAICompatible,
            model: "codellama:13b".to_string(),
            base_url: "http://localhost:11434/v1".to_string(),
            api_key: None,
            context_window: 8192,
            max_tokens: 2048,
            temperature: 0.7,
            timeout_secs: 300,
            network_access: false,
        }
    }
}

impl LlmConfig {
    /// Clamp out-of-range or nonsensical values to safe bounds, emitting
    /// a `tracing::warn!` for each one changed. Called by
    /// `KodConfig::load_default` after deserialization.
    ///
    /// This is intentionally a soft validation: a typo like
    /// `temperature = 2.5` (a user meaning 0.25 or 25% depending on
    /// their convention) currently goes straight to the provider, which
    /// either clamps silently or rejects with an opaque HTTP 400. A
    /// `context_window = 0` reaches `render_history`'s char caps and
    /// the TUI's meter unclamped. Clamping here means the session
    /// still runs, the value in effect is one the user can see in the
    /// warning, and the next prompt has a chance to work.
    ///
    /// Does not change `LlmConfig::default()` — the defaults are already
    /// in range. Only user-supplied configs are affected.
    pub fn validate(&mut self) {
        // Temperature: providers accept roughly 0.0–2.0 (OpenAI's range).
        // A negative value is always wrong; anything above 2.0 is either
        // a typo for a percentage or an attempt the server will reject.
        let t = self.temperature;
        if !t.is_finite() {
            tracing::warn!(
                value = %t,
                "llm.temperature is not finite; falling back to 0.7"
            );
            self.temperature = 0.7;
        } else if t < 0.0 {
            tracing::warn!(
                value = %t,
                "llm.temperature is negative; clamping to 0.0"
            );
            self.temperature = 0.0;
        } else if t > 2.0 {
            tracing::warn!(
                value = %t,
                "llm.temperature > 2.0 is not accepted by most providers; clamping to 2.0"
            );
            self.temperature = 2.0;
        }

        // Context window. Three downstream floors have to be
        // consistent with this value, or the session behaves
        // incoherently:
        //
        //   - `KodApp::set_context_limit` floors the meter at 1000
        //     tokens.
        //   - `KodEngine::set_history_budget` floors the rendered
        //     history at `MIN_HISTORY_CHAR_BUDGET = 4000` chars
        //     (~1000 tokens at 4 chars/token), and the TUI passes
        //     `context_window * 3`.
        //   - `KodApp::maybe_compact` fires when the accumulated
        //     token estimate exceeds 4/5 of the (floored) window.
        //
        // A user who writes `context_window = 500` gets a meter that
        // says 1000, a history budget that clamps to 4000 chars, and
        // an auto-compact threshold based on 1000 — three numbers that
        // all pretend the window is bigger than the user set, and the
        // history budget alone already overshoots the window. The
        // session works but its accounting lies.
        //
        // Clamp to a coherent minimum instead of letting the incoherent
        // case run. The floor is 2000 tokens: high enough that
        // `2000 * 3 = 6000` chars of history clears the engine's 4000
        // char floor with headroom, and the meter's 4/5 threshold
        // (1600 tokens) leaves room for the prompt scaffolding and a
        // tool result or two before compaction fires.
        //
        // 0 is treated separately: a config file that predates the
        // field, or a user who wrote `context_window = 0` meaning
        // "unknown, use the default", gets `LlmConfig::default()`'s
        // 8192 rather than the floor.
        const MIN_CONTEXT_WINDOW: usize = 2_000;
        if self.context_window == 0 {
            tracing::warn!(
                "llm.context_window is 0; falling back to {}",
                LlmConfig::default().context_window
            );
            self.context_window = LlmConfig::default().context_window;
        } else if self.context_window < MIN_CONTEXT_WINDOW {
            tracing::warn!(
                value = self.context_window,
                floor = MIN_CONTEXT_WINDOW,
                "llm.context_window is below the coherent minimum; clamping to {}",
                MIN_CONTEXT_WINDOW
            );
            self.context_window = MIN_CONTEXT_WINDOW;
        }

        // max_tokens == 0 is worse than context_window == 0 — some
        // providers interpret it as "generate nothing", which returns
        // an empty reply that looks like a silent failure. Clamp to the
        // default.
        if self.max_tokens == 0 {
            tracing::warn!(
                "llm.max_tokens is 0; falling back to {}",
                LlmConfig::default().max_tokens
            );
            self.max_tokens = LlmConfig::default().max_tokens;
        }

        // A tiny timeout_secs makes even a cold Ollama start fail before
        // the model finishes loading. 1s is the floor below which the
        // timeout is almost certainly a mistake.
        if self.timeout_secs < 1 {
            tracing::warn!(
                value = self.timeout_secs,
                "llm.timeout_secs < 1; clamping to 1"
            );
            self.timeout_secs = 1;
        }

        // Base URL: an empty string would make every request fail with
        // a confusing DNS error. Fall back to the default and warn.
        if self.base_url.trim().is_empty() {
            tracing::warn!(
                "llm.base_url is empty; falling back to {}",
                LlmConfig::default().base_url
            );
            self.base_url = LlmConfig::default().base_url;
        }

        // Model: empty model name is rejected by every provider.
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
                    "llm.provider names a protocol that kod does not yet speak; \
                     the CLI will send OpenAI-compatible requests to {} and the \
                     server is likely to reject them. Set provider = \"OpenAICompatible\" \
                     for now (Anthropic and Custom support is planned).",
                    self.base_url
                );
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_llm_config() {
        let config = LlmConfig::default();
        assert_eq!(config.provider, ProviderType::OpenAICompatible);
        assert_eq!(config.model, "codellama:13b");
        assert_eq!(config.base_url, "http://localhost:11434/v1");
    }

    #[test]
    fn test_validate_clamps_temperature() {
        let mut c = LlmConfig {
            temperature: 5.0,
            ..LlmConfig::default()
        };
        c.validate();
        assert_eq!(c.temperature, 2.0);

        let mut c = LlmConfig {
            temperature: -1.0,
            ..LlmConfig::default()
        };
        c.validate();
        assert_eq!(c.temperature, 0.0);

        let mut c = LlmConfig {
            temperature: f32::NAN,
            ..LlmConfig::default()
        };
        c.validate();
        assert_eq!(c.temperature, 0.7);

        // A valid value is untouched.
        let mut c = LlmConfig {
            temperature: 0.3,
            ..LlmConfig::default()
        };
        c.validate();
        assert_eq!(c.temperature, 0.3);
    }

    #[test]
    fn test_validate_clamps_zero_context_and_max_tokens() {
        let mut c = LlmConfig {
            context_window: 0,
            max_tokens: 0,
            ..LlmConfig::default()
        };
        c.validate();
        assert_eq!(c.context_window, LlmConfig::default().context_window);
        assert_eq!(c.max_tokens, LlmConfig::default().max_tokens);
    }

    /// A `context_window` below the coherent floor must be clamped up,
    /// not passed through. Regression: a window of 500 produced an
    /// incoherent session where the meter's floor (1000), the history
    /// budget's floor (4000 chars ≈ 1000 tokens), and the auto-compact
    /// threshold (4/5 of the floored window) all disagreed about how
    /// much room the model had.
    #[test]
    fn test_validate_clamps_tiny_context_window() {
        let mut c = LlmConfig {
            context_window: 500,
            ..LlmConfig::default()
        };
        c.validate();
        assert_eq!(
            c.context_window, 2_000,
            "tiny window must clamp to the coherent floor"
        );

        // Boundary: 1999 clamps, 2000 does not.
        let mut c = LlmConfig {
            context_window: 1_999,
            ..LlmConfig::default()
        };
        c.validate();
        assert_eq!(c.context_window, 2_000);

        let mut c = LlmConfig {
            context_window: 2_000,
            ..LlmConfig::default()
        };
        c.validate();
        assert_eq!(c.context_window, 2_000);

        // Above the floor is honored.
        let mut c = LlmConfig {
            context_window: 131_072,
            ..LlmConfig::default()
        };
        c.validate();
        assert_eq!(c.context_window, 131_072);

        // 0 is a special case that maps to the default, not the floor.
        let mut c = LlmConfig {
            context_window: 0,
            ..LlmConfig::default()
        };
        c.validate();
        assert_eq!(c.context_window, LlmConfig::default().context_window);
    }

    #[test]
    fn test_validate_floors_timeout() {
        let mut c = LlmConfig {
            timeout_secs: 0,
            ..LlmConfig::default()
        };
        c.validate();
        assert_eq!(c.timeout_secs, 1);
    }

    #[test]
    fn test_validate_falls_back_on_empty_strings() {
        let mut c = LlmConfig {
            base_url: "   ".to_string(),
            model: "".to_string(),
            ..LlmConfig::default()
        };
        c.validate();
        assert_eq!(c.base_url, LlmConfig::default().base_url);
        assert_eq!(c.model, LlmConfig::default().model);
    }

    #[test]
    fn test_validate_leaves_defaults_alone() {
        // The default config must be fully valid so a first-run
        // generate-defaults session behaves identically after validate.
        let mut c = LlmConfig::default();
        let expected = LlmConfig::default();
        c.validate();
        assert_eq!(c.temperature, expected.temperature);
        assert_eq!(c.context_window, expected.context_window);
        assert_eq!(c.max_tokens, expected.max_tokens);
        assert_eq!(c.timeout_secs, expected.timeout_secs);
        assert_eq!(c.base_url, expected.base_url);
        assert_eq!(c.model, expected.model);
    }

    /// `provider = "OpenAI"` must still deserialize — the OpenAI API
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
    fn test_legacy_ollama_provider_alias() {
        // Config files written before the rename still load.
        let config: LlmConfig = toml::from_str(
            r#"
            provider = "Ollama"
            model = "llama3.1"
            base_url = "http://localhost:11434"
            context_window = 8192
            max_tokens = 2048
            temperature = 0.7
            timeout_secs = 300
            "#,
        )
        .unwrap();
        assert_eq!(config.provider, ProviderType::OpenAICompatible);
    }
}
