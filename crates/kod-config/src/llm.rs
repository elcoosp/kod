//! LLM provider configuration.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
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
