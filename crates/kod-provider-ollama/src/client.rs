//! Ollama HTTP client with connection management and health checking.

use kod_error::{KodError, Result};
use reqwest::Client;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct OllamaClient {
    http: Client,
    base_url: String,
    default_model: String,
    timeout: Duration,
}

impl OllamaClient {
    /// Create a new client for the given Ollama server URL
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            http: Client::builder()
                .timeout(Duration::from_secs(300))
                .pool_max_idle_per_host(10)
                .build()
                .expect("Failed to build HTTP client"),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            default_model: "llama3.2".to_string(),
            timeout: Duration::from_secs(300),
        }
    }

    /// Builder-style method to set the default model
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.default_model = model.into();
        self
    }

    /// Builder-style method to set timeout
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Get the base URL
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Get the default model
    pub fn default_model(&self) -> &str {
        &self.default_model
    }

    /// Create a builder for more complex configuration
    pub fn builder() -> OllamaClientBuilder {
        OllamaClientBuilder::default()
    }

    /// Check if the Ollama server is reachable
    pub async fn health_check(&self) -> Result<()> {
        let url = format!("{}/api/tags", self.base_url);

        let response = self.http
            .get(&url)
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .map_err(|e| KodError::Provider(format!("Ollama server unreachable: {}", e)))?;

        if !response.status().is_success() {
            return Err(KodError::Provider(format!(
                "Ollama health check failed with status: {}",
                response.status()
            )));
        }

        Ok(())
    }

    /// List all available models from the Ollama server
    pub async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        let url = format!("{}/api/tags", self.base_url);

        let response = self.http
            .get(&url)
            .send()
            .await
            .map_err(|e| KodError::Provider(format!("Failed to list models: {}", e)))?;

        let body: serde_json::Value = response
            .json()
            .await
            .map_err(|e| KodError::Provider(format!("Failed to parse model list: {}", e)))?;

        let models = body["models"]
            .as_array()
            .ok_or_else(|| KodError::Provider("Invalid response format from Ollama".to_string()))?;

        let mut result = Vec::new();
        for model in models {
            let name = model["name"]
                .as_str()
                .ok_or_else(|| KodError::Provider("Model missing name field".to_string()))?;

            let size = model["size"].as_u64().unwrap_or(0);
            let modified_at = model["modified_at"].as_str().unwrap_or("").to_string();

            result.push(ModelInfo {
                name: name.to_string(),
                size_bytes: size,
                modified_at,
            });
        }

        Ok(result)
    }

    /// Generate a completion (non-streaming)
    pub async fn generate(&self, request: &crate::generate::GenerateRequest) -> Result<crate::generate::GenerateResponse> {
        let url = self.url("generate");

        let response = self.http
            .post(&url)
            .json(request)
            .timeout(self.timeout)
            .send()
            .await
            .map_err(|e| KodError::Provider(format!("Generate request failed: {}", e)))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(KodError::Provider(format!(
                "Ollama returned error {}: {}",
                status, body
            )));
        }

        let result: crate::generate::GenerateResponse = response
            .json()
            .await
            .map_err(|e| KodError::Provider(format!("Failed to parse response: {}", e)))?;

        Ok(result)
    }

    /// Generate a streaming completion
    pub async fn generate_stream(
        &self,
        request: &crate::generate::GenerateRequest,
    ) -> Result<crate::streaming::GenerationStream> {
        let mut stream_request = request.clone();
        stream_request.stream = true;

        let url = self.url("generate");

        let response = self.http
            .post(&url)
            .json(&stream_request)
            .timeout(self.timeout)
            .send()
            .await
            .map_err(|e| KodError::Provider(format!("Stream request failed: {}", e)))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(KodError::Provider(format!(
                "Ollama stream error {}: {}",
                status, body
            )));
        }

        // Create channel for stream events
        let (tx, rx) = tokio::sync::mpsc::channel(100);

        let mut body = response.bytes_stream();

        tokio::spawn(async move {
            use futures::StreamExt;

            let mut buffer = String::new();

            while let Some(chunk_result) = body.next().await {
                match chunk_result {
                    Ok(bytes) => {
                        buffer.push_str(&String::from_utf8_lossy(&bytes));

                        // Process complete lines
                        while let Some(pos) = buffer.find('\n') {
                            let line: String = buffer[..pos].to_string();
                            buffer = buffer[pos + 1..].to_string();

                            let trimmed = line.trim();
                            if trimmed.is_empty() {
                                continue;
                            }

                            match crate::streaming::parse_stream_line(trimmed) {
                                Ok(event) => {
                                    if tx.send(Ok(event)).await.is_err() {
                                        return; // Receiver dropped
                                    }
                                }
                                Err(e) => {
                                    let _ = tx.send(Err(e)).await;
                                    return;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(KodError::Provider(format!(
                            "Stream error: {}",
                            e
                        )))).await;
                        return;
                    }
                }
            }
        });

        Ok(crate::streaming::GenerationStream::new(rx))
    }

    /// Generate with tool calling support
    pub async fn generate_with_tools(
        &self,
        request: &crate::generate::GenerateRequest,
        tools: &[kod_types::ToolDefinition],
    ) -> Result<(String, Vec<kod_types::ToolCall>)> {
        let url = self.url("generate");

        let mut request_with_tools = serde_json::to_value(request)
            .map_err(|e| KodError::Serialization(e.to_string()))?;

        request_with_tools["tools"] = serde_json::to_value(crate::tools::format_tools_for_ollama(tools))
            .map_err(|e| KodError::Serialization(e.to_string()))?;

        let response = self.http
            .post(&url)
            .json(&request_with_tools)
            .timeout(self.timeout)
            .send()
            .await
            .map_err(|e| KodError::Provider(format!("Generate with tools failed: {}", e)))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(KodError::Provider(format!(
                "Ollama error {}: {}",
                status, body
            )));
        }

        let body: serde_json::Value = response
            .json()
            .await
            .map_err(|e| KodError::Provider(format!("Failed to parse response: {}", e)))?;

        crate::tools::parse_tool_calls(&body)
    }

    /// Get internal HTTP client (for advanced use cases)
    #[allow(dead_code)]
    pub(crate) fn http(&self) -> &Client {
        &self.http
    }

    /// Construct a full URL for an API endpoint
    pub(crate) fn url(&self, endpoint: &str) -> String {
        format!("{}/api/{}", self.base_url, endpoint)
    }
}

/// Builder for OllamaClient with more configuration options
#[derive(Debug, Default)]
pub struct OllamaClientBuilder {
    base_url: Option<String>,
    model: Option<String>,
    timeout_secs: Option<u64>,
}

impl OllamaClientBuilder {
    pub fn base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = Some(url.into());
        self
    }

    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    pub fn timeout_secs(mut self, secs: u64) -> Self {
        self.timeout_secs = Some(secs);
        self
    }

    pub fn build(self) -> OllamaClient {
        let mut client = OllamaClient::new(
            self.base_url.unwrap_or_else(|| "http://localhost:11434".to_string())
        );

        if let Some(model) = self.model {
            client = client.with_model(model);
        }

        if let Some(secs) = self.timeout_secs {
            client = client.with_timeout(Duration::from_secs(secs));
        }

        client
    }
}

/// Information about an available Ollama model
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ModelInfo {
    pub name: String,
    pub size_bytes: u64,
    pub modified_at: String,
}

impl ModelInfo {
    /// Get human-readable size
    pub fn size_human(&self) -> String {
        let size = self.size_bytes as f64;
        let units = ["B", "KB", "MB", "GB", "TB"];
        let mut unit_index = 0;
        let mut size = size;

        while size >= 1024.0 && unit_index < units.len() - 1 {
            size /= 1024.0;
            unit_index += 1;
        }

        if unit_index == 0 {
            format!("{} {}", self.size_bytes, units[unit_index])
        } else {
            format!("{:.2} {}", size, units[unit_index])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_url_construction() {
        let client = OllamaClient::new("http://localhost:11434/");
        assert_eq!(client.url("generate"), "http://localhost:11434/api/generate");
        assert_eq!(client.url("tags"), "http://localhost:11434/api/tags");
    }

    #[test]
    fn test_base_url_trailing_slash() {
        let client = OllamaClient::new("http://localhost:11434///");
        assert_eq!(client.base_url(), "http://localhost:11434");
    }

    #[test]
    fn test_size_human_readable() {
        let model = ModelInfo {
            name: "test".to_string(),
            size_bytes: 1024 * 1024 * 1024, // 1GB
            modified_at: "2026-01-01".to_string(),
        };
        assert_eq!(model.size_human(), "1.00 GB");

        let small_model = ModelInfo {
            name: "test".to_string(),
            size_bytes: 512,
            modified_at: "2026-01-01".to_string(),
        };
        assert_eq!(small_model.size_human(), "512 B");
    }
}
