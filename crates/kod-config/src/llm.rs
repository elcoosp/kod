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

        // Context window: the codebase's own caps (`render_history`'s
        // MAX_HISTORY_CHARS, the TUI meter's floor, per-tool prompt
        // caps) all assume a positive window. 0 means "unknown" to some
        // users, but every consumer here treats it as "0 tokens
        // available", which then silently discards every memory and
        // history entry. Clamp to the default.
        if self.context_window == 0 {
            tracing::warn!(
                "llm.context_window is 0; falling back to {}",
                LlmConfig::default().context_window
            );
            self.context_window = LlmConfig::default().context_window;
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
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ProviderType {
    /// Any OpenAI-spec chat-completions endpoint (Ollama `/v1`, LM Studio,
    /// MLX Omni Serve, vLLM, OpenAI). `Ollama` is kept as a deprecated alias
    /// so existing config files keep loading.
    #[serde(alias = "Ollama")]
    OpenAICompatible,
    Anthropic,
    OpenAI,
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
