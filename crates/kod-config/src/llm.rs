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
            provider: ProviderType::Ollama,
            model: "codellama:13b".to_string(),
            base_url: "http://localhost:11434".to_string(),
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
    Ollama,
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
        assert_eq!(config.provider, ProviderType::Ollama);
        assert_eq!(config.model, "codellama:13b");
        assert_eq!(config.base_url, "http://localhost:11434");
    }
}
