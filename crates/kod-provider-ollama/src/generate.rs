//! Request and response types for Ollama generation API.

use serde::{Deserialize, Serialize};

/// Request to generate a completion from Ollama
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenerateRequest {
    pub model: String,
    pub prompt: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<Vec<i32>>,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<GenerateOptions>,
}

impl GenerateRequest {
    /// Create a new generation request
    pub fn new(model: impl Into<String>, prompt: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            prompt: prompt.into(),
            system: None,
            template: None,
            context: None,
            stream: false,
            options: None,
        }
    }

    /// Set a system prompt
    pub fn with_system(mut self, system: impl Into<String>) -> Self {
        self.system = Some(system.into());
        self
    }

    /// Set generation options
    pub fn with_options(mut self, options: GenerateOptions) -> Self {
        self.options = Some(options);
        self
    }

    /// Set temperature (convenience method)
    pub fn with_temperature(mut self, temperature: f32) -> Self {
        let options = self.options.get_or_insert_with(GenerateOptions::default);
        options.temperature = Some(temperature);
        self
    }

    /// Set max tokens (convenience method)
    pub fn with_max_tokens(mut self, max_tokens: usize) -> Self {
        let options = self.options.get_or_insert_with(GenerateOptions::default);
        options.num_predict = Some(max_tokens as i32);
        self
    }

    /// Set stop sequences
    pub fn with_stop_sequences(mut self, stops: Vec<String>) -> Self {
        let options = self.options.get_or_insert_with(GenerateOptions::default);
        options.stop = Some(stops);
        self
    }

    /// Enable or disable streaming
    pub fn with_stream(mut self, stream: bool) -> Self {
        self.stream = stream;
        self
    }
}

/// Options for controlling generation behavior
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GenerateOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_k: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub num_predict: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub num_ctx: Option<i32>,
}

/// Response from Ollama generation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenerateResponse {
    pub model: String,
    pub response: String,
    pub done: bool,
    #[serde(default, rename = "total_duration")]
    pub total_duration_ns: u64,
    #[serde(default, rename = "load_duration")]
    pub load_duration_ns: u64,
    #[serde(default, rename = "prompt_eval_count")]
    pub prompt_eval_count: i32,
    #[serde(default, rename = "prompt_eval_duration")]
    pub prompt_eval_duration_ns: u64,
    #[serde(default, rename = "eval_count")]
    pub eval_count: i32,
    #[serde(default, rename = "eval_duration")]
    pub eval_duration_ns: u64,
}

impl GenerateResponse {
    /// Calculate tokens per second (generation speed)
    pub fn tokens_per_second(&self) -> f64 {
        if self.eval_duration_ns == 0 {
            return 0.0;
        }
        let seconds = self.eval_duration_ns as f64 / 1_000_000_000.0;
        self.eval_count as f64 / seconds
    }

    /// Calculate total time in milliseconds
    pub fn total_time_ms(&self) -> u64 {
        self.total_duration_ns / 1_000_000
    }

    /// Get prompt token count
    pub fn prompt_tokens(&self) -> i32 {
        self.prompt_eval_count
    }

    /// Get completion token count
    pub fn completion_tokens(&self) -> i32 {
        self.eval_count
    }
}

/// Streaming chunk from Ollama
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamChunkData {
    pub model: String,
    #[serde(default)]
    pub response: String,
    pub done: bool,
    #[serde(default)]
    pub total_duration_ns: Option<u64>,
    #[serde(default)]
    pub eval_count: Option<i32>,
    #[serde(default)]
    pub eval_duration_ns: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_request_serialization() {
        let request = GenerateRequest::new("llama3.2", "Hello");
        let json = serde_json::to_value(&request).unwrap();

        assert_eq!(json["model"], "llama3.2");
        assert_eq!(json["prompt"], "Hello");
        assert_eq!(json["stream"], false);
        assert!(json.get("system").is_none());
        assert!(json.get("options").is_none());
    }

    #[test]
    fn test_request_with_all_options() {
        let request = GenerateRequest::new("llama3.2", "Hello")
            .with_system("You are helpful")
            .with_temperature(0.7)
            .with_max_tokens(100);

        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["system"], "You are helpful");
        assert!((json["options"]["temperature"].as_f64().unwrap() - 0.7).abs() < 0.001);
        assert_eq!(json["options"]["num_predict"], 100);
    }

    #[test]
    fn test_response_deserialization() {
        let json = serde_json::json!({
            "model": "llama3.2",
            "response": "Hello!",
            "done": true,
            "total_duration": 1000000000,
            "eval_count": 10,
            "eval_duration": 500000000
        });

        let response: GenerateResponse = serde_json::from_value(json).unwrap();
        assert_eq!(response.eval_count, 10);
        assert_eq!(response.tokens_per_second(), 20.0); // 10 / 0.5s
    }
}
