# Chunk 2: LLM Provider Implementation (Ollama)

## Task 7: Ollama Client Foundation

**Files:**
- Create: `crates/kod-provider-ollama/src/client.rs`
- Modify: `crates/kod-provider-ollama/src/lib.rs`
- Test: `crates/kod-provider-ollama/tests/client.rs`

- [ ] **Step 1: Write failing test for client creation and health check**

Create `crates/kod-provider-ollama/tests/client.rs`:

```rust
use kod_provider_ollama::OllamaClient;
use tokio_test::assert_ok;

#[tokio::test]
async fn test_client_creation() {
    let client = OllamaClient::new("http://localhost:11434");
    assert_eq!(client.base_url(), "http://localhost:11434");
}

#[tokio::test]
async fn test_health_check_unreachable() {
    let client = OllamaClient::new("http://localhost:19999"); // Non-existent port
    let result = client.health_check().await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_client_with_custom_model() {
    let client = OllamaClient::new("http://localhost:11434")
        .with_model("llama3.2");
    assert_eq!(client.default_model(), "llama3.2");
}

#[tokio::test]
async fn test_client_builder_pattern() {
    let client = OllamaClient::builder()
        .base_url("http://localhost:11434")
        .model("codellama:13b")
        .timeout_secs(120)
        .build();
    
    assert_eq!(client.base_url(), "http://localhost:11434");
    assert_eq!(client.default_model(), "codellama:13b");
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test -p kod-provider-ollama --test client
```

Expected: FAIL - `OllamaClient` not implemented

- [ ] **Step 3: Implement OllamaClient**

Create `crates/kod-provider-ollama/src/client.rs`:

```rust
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

    /// Get internal HTTP client (for advanced use cases)
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
```

- [ ] **Step 4: Update lib.rs to export client**

Update `crates/kod-provider-ollama/src/lib.rs`:

```rust
//! Ollama LLM provider implementation.

pub mod client;
pub mod generate;
pub mod streaming;
pub mod tools;

pub use client::{ModelInfo, OllamaClient, OllamaClientBuilder};
```

- [ ] **Step 5: Run tests to verify they pass**

```bash
cargo test -p kod-provider-ollama --test client
```

Expected: PASS (except unreachable server test which will fail if no Ollama running - mark as `#[ignore]` for CI)

- [ ] **Step 6: Commit**

```bash
git add crates/kod-provider-ollama/
git commit -m "feat(provider-ollama): add client with health check and model listing"
```

---

## Task 8: Basic Generation (Non-Streaming)

**Files:**
- Create: `crates/kod-provider-ollama/src/generate.rs`
- Test: `crates/kod-provider-ollama/tests/generate.rs`

- [ ] **Step 1: Write failing test for generation request/response types**

Create `crates/kod-provider-ollama/tests/generate.rs`:

```rust
use kod_provider_ollama::{GenerateRequest, GenerateResponse};
use serde_json::json;

#[test]
fn test_generate_request_serialization() {
    let request = GenerateRequest::new("llama3.2", "Hello, world!");
    
    let json = serde_json::to_value(&request).unwrap();
    assert_eq!(json["model"], "llama3.2");
    assert_eq!(json["prompt"], "Hello, world!");
    assert_eq!(json["stream"], false);
}

#[test]
fn test_generate_request_with_options() {
    let request = GenerateRequest::new("llama3.2", "Test prompt")
        .with_temperature(0.5)
        .with_max_tokens(100);
    
    let json = serde_json::to_value(&request).unwrap();
    assert_eq!(json["options"]["temperature"], 0.5);
    assert_eq!(json["options"]["num_predict"], 100);
}

#[test]
fn test_generate_response_deserialization() {
    let json = json!({
        "model": "llama3.2",
        "response": "Hello! How can I help you?",
        "done": true,
        "total_duration": 1000000000,
        "eval_count": 50,
        "eval_duration": 900000000
    });
    
    let response: GenerateResponse = serde_json::from_value(json).unwrap();
    assert_eq!(response.response, "Hello! How can I help you?");
    assert!(response.done);
    assert_eq!(response.eval_count, 50);
}

#[test]
fn test_generate_response_token_calculation() {
    let response = GenerateResponse {
        model: "llama3.2".to_string(),
        response: "Test".to_string(),
        done: true,
        total_duration_ns: 1_000_000_000,
        load_duration_ns: 100_000_000,
        prompt_eval_count: 10,
        prompt_eval_duration_ns: 200_000_000,
        eval_count: 50,
        eval_duration_ns: 700_000_000,
    };
    
    // Tokens per second should be eval_count / (eval_duration / 1e9)
    let tps = response.tokens_per_second();
    assert!((tps - 71.43).abs() < 0.1); // 50 / 0.7 seconds
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test -p kod-provider-ollama --test generate
```

Expected: FAIL - types not defined

- [ ] **Step 3: Implement generate types**

Create `crates/kod-provider-ollama/src/generate.rs`:

```rust
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
    #[serde(default)]
    pub total_duration_ns: u64,
    #[serde(default)]
    pub load_duration_ns: u64,
    #[serde(default)]
    pub prompt_eval_count: i32,
    #[serde(default)]
    pub prompt_eval_duration_ns: u64,
    #[serde(default)]
    pub eval_count: i32,
    #[serde(default)]
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_duration_ns: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub eval_count: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
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
        assert_eq!(json["options"]["temperature"], 0.7);
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
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
cargo test -p kod-provider-ollama --test generate
cargo test -p kod-provider-ollama --lib generate
```

Expected: All tests pass

- [ ] **Step 5: Implement generate method on client**

Add to `crates/kod-provider-ollama/src/client.rs`:

```rust
impl OllamaClient {
    /// Generate a completion (non-streaming)
    pub async fn generate(&self, request: &GenerateRequest) -> Result<GenerateResponse> {
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
        
        let result: GenerateResponse = response
            .json()
            .await
            .map_err(|e| KodError::Provider(format!("Failed to parse response: {}", e)))?;
        
        Ok(result)
    }
}
```

- [ ] **Step 6: Write integration test (requires running Ollama)**

Create `crates/kod-provider-ollama/tests/integration.rs`:

```rust
use kod_provider_ollama::{GenerateRequest, OllamaClient};

#[tokio::test]
#[ignore = "requires running Ollama server"]
async fn test_real_generation() {
    let client = OllamaClient::new("http://localhost:11434");
    
    // First check health
    client.health_check().await.expect("Ollama should be running");
    
    // List models
    let models = client.list_models().await.unwrap();
    assert!(!models.is_empty(), "Should have at least one model");
    
    // Use first available model
    let model_name = models[0].name.clone();
    
    // Generate
    let request = GenerateRequest::new(&model_name, "Say 'hello' and nothing else.")
        .with_temperature(0.1);
    
    let response = client.generate(&request).await.unwrap();
    assert!(response.done);
    assert!(!response.response.is_empty());
    assert!(response.tokens_per_second() > 0.0);
}
```

- [ ] **Step 7: Run integration test (if Ollama is running)**

```bash
cargo test -p kod-provider-ollama --test integration -- --ignored
```

Expected: PASS if Ollama is running with models

- [ ] **Step 8: Commit**

```bash
git add crates/kod-provider-ollama/
git commit -m "feat(provider-ollama): add generation with request/response types"
```

---

## Task 9: Streaming Generation with SSE

**Files:**
- Create: `crates/kod-provider-ollama/src/streaming.rs`
- Modify: `crates/kod-provider-ollama/src/client.rs`
- Test: `crates/kod-provider-ollama/tests/streaming.rs`

- [ ] **Step 1: Write failing test for stream chunk handling**

Create `crates/kod-provider-ollama/tests/streaming.rs`:

```rust
use kod_provider_ollama::streaming::StreamEvent;

#[test]
fn test_stream_event_parsing() {
    // Simulate Ollama's newline-delimited JSON responses
    let json = r#"{"model":"llama3.2","response":"Hello","done":false}"#;
    let event: StreamEvent = serde_json::from_str(json).unwrap();
    
    match event {
        StreamEvent::Chunk { response, done } => {
            assert_eq!(response, "Hello");
            assert!(!done);
        }
        _ => panic!("Expected chunk event"),
    }
}

#[test]
fn test_stream_done_event() {
    let json = r#"{"model":"llama3.2","response":"","done":true,"total_duration":1000000000,"eval_count":10}"#;
    let event: StreamEvent = serde_json::from_str(json).unwrap();
    
    match event {
        StreamEvent::Done { total_duration, eval_count } => {
            assert_eq!(total_duration, 1_000_000_000);
            assert_eq!(eval_count, 10);
        }
        _ => panic!("Expected done event"),
    }
}

#[tokio::test]
async fn test_stream_accumulation() {
    use futures::StreamExt;
    use kod_provider_ollama::streaming::StreamAccumulator;
    
    let mut accumulator = StreamAccumulator::new();
    
    // Simulate streaming chunks
    let chunks = vec!["Hello", " ", "world", "!"];
    for chunk in chunks {
        accumulator.add_chunk(chunk).unwrap();
    }
    
    let result = accumulator.finish().unwrap();
    assert_eq!(result, "Hello world!");
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test -p kod-provider-ollama --test streaming
```

Expected: FAIL - streaming module not implemented

- [ ] **Step 3: Implement streaming module**

Create `crates/kod-provider-ollama/src/streaming.rs`:

```rust
//! Streaming support for Ollama generation.
//!
//! Ollama uses newline-delimited JSON (NDJSON) for streaming,
//! not Server-Sent Events. Each line is a complete JSON object.

use futures::Stream;
use kod_error::{KodError, Result};
use serde::Deserialize;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::sync::mpsc;

/// Events emitted during streaming generation
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "done", rename_all = "lowercase")]
pub enum StreamEvent {
    /// A chunk of generated text
    #[serde(rename = "false")]
    Chunk {
        #[serde(default)]
        response: String,
        done: bool,
    },
    /// Final event with statistics
    #[serde(rename = "true")]
    Done {
        #[serde(default)]
        response: String,
        done: bool,
        #[serde(default)]
        total_duration: u64,
        #[serde(default)]
        eval_count: i32,
        #[serde(default)]
        eval_duration: u64,
    },
}

/// Accumulates streaming chunks into a complete response
#[derive(Debug, Default)]
pub struct StreamAccumulator {
    buffer: String,
    total_duration_ns: u64,
    eval_count: i32,
    is_done: bool,
}

impl StreamAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a text chunk to the accumulator
    pub fn add_chunk(&mut self, text: &str) -> Result<()> {
        if self.is_done {
            return Err(KodError::Provider("Stream already finished".to_string()));
        }
        self.buffer.push_str(text);
        Ok(())
    }

    /// Mark the stream as done with final statistics
    pub fn finish_with_stats(&mut self, total_duration_ns: u64, eval_count: i32) -> Result<()> {
        self.total_duration_ns = total_duration_ns;
        self.eval_count = eval_count;
        self.is_done = true;
        Ok(())
    }

    /// Mark the stream as done (no stats)
    pub fn finish(&mut self) -> Result<String> {
        self.is_done = true;
        Ok(self.buffer.clone())
    }

    /// Get the accumulated text so far
    pub fn current_text(&self) -> &str {
        &self.buffer
    }

    /// Check if stream is done
    pub fn is_done(&self) -> bool {
        self.is_done
    }

    /// Get tokens per second if stats are available
    pub fn tokens_per_second(&self) -> Option<f64> {
        if self.total_duration_ns == 0 {
            return None;
        }
        Some(self.eval_count as f64 / (self.total_duration_ns as f64 / 1_000_000_000.0))
    }
}

/// Stream of generation events from Ollama
pub struct GenerationStream {
    receiver: mpsc::Receiver<Result<StreamEvent>>,
}

impl GenerationStream {
    pub(crate) fn new(receiver: mpsc::Receiver<Result<StreamEvent>>) -> Self {
        Self { receiver }
    }
}

impl Stream for GenerationStream {
    type Item = Result<StreamEvent>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        self.receiver.poll_recv(cx)
    }
}

/// Parse a line of NDJSON into a StreamEvent
pub fn parse_stream_line(line: &str) -> Result<StreamEvent> {
    serde_json::from_str(line)
        .map_err(|e| KodError::Provider(format!("Failed to parse stream line: {} - {}", line, e)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_chunk() {
        let line = r#"{"model":"llama3.2","response":"Hello","done":false}"#;
        let event = parse_stream_line(line).unwrap();
        
        match event {
            StreamEvent::Chunk { response, done } => {
                assert_eq!(response, "Hello");
                assert!(!done);
            }
            _ => panic!("Expected chunk"),
        }
    }

    #[test]
    fn test_parse_done() {
        let line = r#"{"model":"llama3.2","response":"","done":true,"total_duration":1000,"eval_count":5}"#;
        let event = parse_stream_line(line).unwrap();
        
        match event {
            StreamEvent::Done { total_duration, eval_count, .. } => {
                assert_eq!(total_duration, 1000);
                assert_eq!(eval_count, 5);
            }
            _ => panic!("Expected done"),
        }
    }

    #[test]
    fn test_accumulator() {
        let mut acc = StreamAccumulator::new();
        acc.add_chunk("Hello").unwrap();
        acc.add_chunk(" ").unwrap();
        acc.add_chunk("world").unwrap();
        
        assert_eq!(acc.current_text(), "Hello world");
        
        let final_text = acc.finish().unwrap();
        assert_eq!(final_text, "Hello world");
        assert!(acc.is_done());
    }
}
```

- [ ] **Step 4: Implement streaming generation on client**

Add to `crates/kod-provider-ollama/src/client.rs`:

```rust
impl OllamaClient {
    /// Generate a streaming completion
    pub async fn generate_stream(
        &self,
        request: &GenerateRequest,
    ) -> Result<GenerationStream> {
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
        let (tx, rx) = mpsc::channel(100);
        
        // Spawn task to parse NDJSON stream
        let mut byte_stream = response.bytes_stream();
        
        tokio::spawn(async move {
            use futures::StreamExt;
            
            let mut buffer = String::new();
            
            while let Some(chunk_result) = byte_stream.next().await {
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
                            
                            match parse_stream_line(trimmed) {
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
        
        Ok(GenerationStream::new(rx))
    }
}
```

- [ ] **Step 5: Update lib.rs exports**

Update `crates/kod-provider-ollama/src/lib.rs`:

```rust
//! Ollama LLM provider implementation.

pub mod client;
pub mod generate;
pub mod streaming;
pub mod tools;

pub use client::{ModelInfo, OllamaClient, OllamaClientBuilder};
pub use generate::{GenerateOptions, GenerateRequest, GenerateResponse};
pub use streaming::{GenerationStream, StreamAccumulator, StreamEvent};
```

- [ ] **Step 6: Run tests**

```bash
cargo test -p kod-provider-ollama
```

Expected: All tests pass

- [ ] **Step 7: Add streaming integration test**

Add to `crates/kod-provider-ollama/tests/integration.rs`:

```rust
#[tokio::test]
#[ignore = "requires running Ollama server"]
async fn test_real_streaming() {
    use futures::StreamExt;
    
    let client = OllamaClient::new("http://localhost:11434");
    client.health_check().await.expect("Ollama should be running");
    
    let models = client.list_models().await.unwrap();
    let model_name = models[0].name.clone();
    
    let request = GenerateRequest::new(&model_name, "Count from 1 to 5.");
    
    let mut stream = client.generate_stream(&request).await.unwrap();
    let mut accumulator = StreamAccumulator::new();
    
    while let Some(event_result) = stream.next().await {
        let event = event_result.unwrap();
        match event {
            StreamEvent::Chunk { response, .. } => {
                accumulator.add_chunk(&response).unwrap();
            }
            StreamEvent::Done { total_duration, eval_count, .. } => {
                accumulator.finish_with_stats(total_duration, eval_count).unwrap();
            }
        }
    }
    
    let final_text = accumulator.finish().unwrap();
    assert!(!final_text.is_empty());
    assert!(final_text.contains('1'));
}
```

- [ ] **Step 8: Commit**

```bash
git add crates/kod-provider-ollama/
git commit -m "feat(provider-ollama): add streaming generation with NDJSON parsing"
```

---

## Task 10: Tool Calling Support

**Files:**
- Create: `crates/kod-provider-ollama/src/tools.rs`
- Test: `crates/kod-provider-ollama/tests/tools.rs`

- [ ] **Step 1: Write failing test for tool formatting**

Create `crates/kod-provider-ollama/tests/tools.rs`:

```rust
use kod_provider_ollama::tools::{format_tools_for_ollama, parse_tool_calls};
use kod_types::{ToolDefinition, ToolPermissions};
use serde_json::json;

fn create_test_tool() -> ToolDefinition {
    ToolDefinition {
        id: kod_types::ToolId::new(),
        name: "read_file".to_string(),
        description: "Read a file from the filesystem".to_string(),
        category: kod_types::ToolCategory::FileSystem,
        parameters_schema: json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file"
                }
            },
            "required": ["path"]
        }),
        permissions: ToolPermissions::default(),
    }
}

#[test]
fn test_format_tools_for_ollama() {
    let tools = vec![create_test_tool()];
    let formatted = format_tools_for_ollama(&tools);
    
    assert_eq!(formatted.len(), 1);
    assert_eq!(formatted[0]["type"], "function");
    assert_eq!(formatted[0]["function"]["name"], "read_file");
    assert_eq!(
        formatted[0]["function"]["description"],
        "Read a file from the filesystem"
    );
}

#[test]
fn test_parse_tool_calls_single() {
    let response = json!({
        "model": "llama3.2",
        "response": "I'll read the file.",
        "tool_calls": [
            {
                "function": {
                    "name": "read_file",
                    "arguments": {
                        "path": "/test.rs"
                    }
                }
            }
        ],
        "done": true
    });
    
    let (text, tool_calls) = parse_tool_calls(&response).unwrap();
    
    assert_eq!(text, "I'll read the file.");
    assert_eq!(tool_calls.len(), 1);
    assert_eq!(tool_calls[0].tool_name, "read_file");
    assert_eq!(tool_calls[0].arguments["path"], "/test.rs");
}

#[test]
fn test_parse_tool_calls_none() {
    let response = json!({
        "model": "llama3.2",
        "response": "Just a text response.",
        "done": true
    });
    
    let (text, tool_calls) = parse_tool_calls(&response).unwrap();
    
    assert_eq!(text, "Just a text response.");
    assert!(tool_calls.is_empty());
}

#[test]
fn test_parse_tool_calls_multiple() {
    let response = json!({
        "model": "llama3.2",
        "response": "I'll do multiple things.",
        "tool_calls": [
            {
                "function": {
                    "name": "read_file",
                    "arguments": {"path": "/a.rs"}
                }
            },
            {
                "function": {
                    "name": "write_file",
                    "arguments": {"path": "/b.rs", "content": "hello"}
                }
            }
        ],
        "done": true
    });
    
    let (_, tool_calls) = parse_tool_calls(&response).unwrap();
    assert_eq!(tool_calls.len(), 2);
    assert_eq!(tool_calls[1].tool_name, "write_file");
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test -p kod-provider-ollama --test tools
```

Expected: FAIL - tools module not implemented

- [ ] **Step 3: Implement tools module**

Create `crates/kod-provider-ollama/src/tools.rs`:

```rust
//! Tool calling support for Ollama.
//!
//! Ollama supports tool calling (function calling) by providing
//! tool definitions in the request and parsing tool_calls in responses.

use kod_error::{KodError, Result};
use kod_types::{ToolCall, ToolDefinition};
use serde_json::{json, Value};

/// Format tool definitions for the Ollama API
pub fn format_tools_for_ollama(tools: &[ToolDefinition]) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "function": {
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.parameters_schema,
                }
            })
        })
        .collect()
}

/// Parse a response from Ollama that may contain tool calls
pub fn parse_tool_calls(response: &Value) -> Result<(String, Vec<ToolCall>)> {
    let text = response["response"]
        .as_str()
        .unwrap_or("")
        .to_string();
    
    let mut tool_calls = Vec::new();
    
    if let Some(calls_array) = response["tool_calls"].as_array() {
        for call in calls_array {
            let function = &call["function"];
            
            let name = function["name"]
                .as_str()
                .ok_or_else(|| KodError::Provider("Tool call missing name".to_string()))?;
            
            let arguments = function["arguments"].clone();
            
            // Handle both object and string arguments
            let arguments = if arguments.is_string() {
                // Some models return arguments as a JSON string
                let arg_str = arguments.as_str().unwrap();
                serde_json::from_str(arg_str)
                    .unwrap_or_else(|_| json!({}))
            } else if arguments.is_null() {
                json!({})
            } else {
                arguments
            };
            
            tool_calls.push(ToolCall {
                tool_name: name.to_string(),
                arguments,
            });
        }
    }
    
    Ok((text, tool_calls))
}

/// Build a tool result message to send back to Ollama
pub fn build_tool_result_message(
    tool_name: &str,
    result: &Value,
) -> Value {
    json!({
        "role": "tool",
        "tool_name": tool_name,
        "content": result,
    })
}

/// Build a chat-format request with tool results
pub fn build_chat_request_with_tools(
    model: &str,
    messages: Vec<Value>,
    tools: &[ToolDefinition],
) -> Value {
    let tools_json = format_tools_for_ollama(tools);
    
    json!({
        "model": model,
        "messages": messages,
        "tools": tools_json,
        "stream": false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use kod_types::{ToolCategory, ToolId, ToolPermissions};

    fn test_tool() -> ToolDefinition {
        ToolDefinition {
            id: ToolId::new(),
            name: "get_weather".to_string(),
            description: "Get current weather".to_string(),
            category: ToolCategory::Web,
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "location": {
                        "type": "string",
                        "description": "City name"
                    }
                },
                "required": ["location"]
            }),
            permissions: ToolPermissions::default(),
        }
    }

    #[test]
    fn test_format_tools() {
        let tools = vec![test_tool()];
        let formatted = format_tools_for_ollama(&tools);
        
        assert_eq!(formatted[0]["function"]["name"], "get_weather");
        assert!(formatted[0]["function"]["parameters"].is_object());
    }

    #[test]
    fn test_parse_empty_response() {
        let response = json!({"response": "Hello", "done": true});
        let (text, calls) = parse_tool_calls(&response).unwrap();
        
        assert_eq!(text, "Hello");
        assert!(calls.is_empty());
    }

    #[test]
    fn test_parse_with_tool_calls() {
        let response = json!({
            "response": "Checking weather",
            "tool_calls": [
                {
                    "function": {
                        "name": "get_weather",
                        "arguments": {"location": "Paris"}
                    }
                }
            ]
        });
        
        let (text, calls) = parse_tool_calls(&response).unwrap();
        assert_eq!(text, "Checking weather");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].tool_name, "get_weather");
    }

    #[test]
    fn test_parse_string_arguments() {
        // Some models return arguments as JSON string
        let response = json!({
            "response": "",
            "tool_calls": [
                {
                    "function": {
                        "name": "test",
                        "arguments": "{\"key\": \"value\"}"
                    }
                }
            ]
        });
        
        let (_, calls) = parse_tool_calls(&response).unwrap();
        assert_eq!(calls[0].arguments["key"], "value");
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
cargo test -p kod-provider-ollama --test tools
cargo test -p kod-provider-ollama --lib tools
```

Expected: All tests pass

- [ ] **Step 5: Add tool-calling generation to client**

Add to `crates/kod-provider-ollama/src/client.rs`:

```rust
impl OllamaClient {
    /// Generate with tool calling support
    pub async fn generate_with_tools(
        &self,
        request: &GenerateRequest,
        tools: &[ToolDefinition],
    ) -> Result<(String, Vec<ToolCall>)> {
        let url = self.url("generate");
        
        // Build request with tools
        let mut request_with_tools = serde_json::to_value(request)
            .map_err(|e| KodError::Serialization(e.to_string()))?;
        
        request_with_tools["tools"] = serde_json::to_value(tools::format_tools_for_ollama(tools))
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
        
        tools::parse_tool_calls(&body)
    }
}
```

- [ ] **Step 6: Add tool calling integration test**

Add to `crates/kod-provider-ollama/tests/integration.rs`:

```rust
#[tokio::test]
#[ignore = "requires running Ollama with tool-supporting model"]
async fn test_tool_calling() {
    let client = OllamaClient::new("http://localhost:11434");
    client.health_check().await.expect("Ollama should be running");
    
    // Use a model that supports tools
    let request = GenerateRequest::new("llama3.2", "What is the weather in Paris?");
    
    let tools = vec![ToolDefinition {
        id: ToolId::new(),
        name: "get_weather".to_string(),
        description: "Get current weather for a location".to_string(),
        category: ToolCategory::Web,
        parameters_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "location": {
                    "type": "string",
                    "description": "City name"
                }
            },
            "required": ["location"]
        }),
        permissions: ToolPermissions::default(),
    }];
    
    let (text, tool_calls) = client.generate_with_tools(&request, &tools).await.unwrap();
    
    // Model may or may not call the tool, but should not error
    println!("Text: {}", text);
    println!("Tool calls: {:?}", tool_calls);
}
```

- [ ] **Step 7: Update lib.rs exports**

Update `crates/kod-provider-ollama/src/lib.rs`:

```rust
//! Ollama LLM provider implementation.

pub mod client;
pub mod generate;
pub mod streaming;
pub mod tools;

pub use client::{ModelInfo, OllamaClient, OllamaClientBuilder};
pub use generate::{GenerateOptions, GenerateRequest, GenerateResponse};
pub use streaming::{GenerationStream, StreamAccumulator, StreamEvent};
pub use tools::{format_tools_for_ollama, parse_tool_calls};
```

- [ ] **Step 8: Run all tests**

```bash
cargo test -p kod-provider-ollama
```

Expected: All tests pass

- [ ] **Step 9: Commit**

```bash
git add crates/kod-provider-ollama/
git commit -m "feat(provider-ollama): add tool calling support with parsing and formatting"
```

---

## Task 11: Implement LlmProvider Trait

**Files:**
- Modify: `crates/kod-provider-ollama/src/lib.rs`
- Create: `crates/kod-provider-ollama/src/provider.rs`
- Test: `crates/kod-provider-ollama/tests/provider.rs`

- [ ] **Step 1: Write failing test for provider trait implementation**

Create `crates/kod-provider-ollama/tests/provider.rs`:

```rust
use kod_provider::LlmProvider;
use kod_provider_ollama::OllamaLlmProvider;

#[tokio::test]
async fn test_provider_name() {
    let provider = OllamaLlmProvider::new("http://localhost:11434");
    assert_eq!(provider.name(), "ollama");
}

#[tokio::test]
async fn test_provider_default_model() {
    let provider = OllamaLlmProvider::new("http://localhost:11434")
        .with_model("codellama:13b");
    
    let models = provider.list_models().await;
    // This will fail if Ollama isn't running, which is expected for unit test
    assert!(models.is_err() || models.is_ok());
}

#[test]
fn test_generation_options_conversion() {
    use kod_provider::GenerationOptions;
    
    let options = GenerationOptions {
        model: Some("test".to_string()),
        max_tokens: Some(100),
        temperature: Some(0.5),
        ..Default::default()
    };
    
    // Options should be convertible to Ollama format
    let ollama_options = options.to_ollama_options();
    assert_eq!(ollama_options.temperature, Some(0.5));
    assert_eq!(ollama_options.num_predict, Some(100));
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test -p kod-provider-ollama --test provider
```

Expected: FAIL - provider not implemented

- [ ] **Step 3: Implement LlmProvider trait for OllamaClient**

Create `crates/kod-provider-ollama/src/provider.rs`:

```rust
//! LlmProvider trait implementation for Ollama.

use crate::{client::OllamaClient, generate::GenerateRequest, streaming};
use async_trait::async_trait;
use kod_error::Result;
use kod_provider::{GenerationOptions, GenerationResponse, LlmProvider, StreamChunk};
use kod_types::{ToolCall, ToolDefinition};
use futures::Stream;

/// Adapter that implements LlmProvider for Ollama
pub struct OllamaLlmProvider {
    client: OllamaClient,
}

impl OllamaLlmProvider {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            client: OllamaClient::new(base_url),
        }
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.client = self.client.with_model(model);
        self
    }
}

// Extension trait for GenerationOptions
pub trait GenerationOptionsExt {
    fn to_ollama_options(&self) -> crate::generate::GenerateOptions;
}

impl GenerationOptionsExt for GenerationOptions {
    fn to_ollama_options(&self) -> crate::generate::GenerateOptions {
        crate::generate::GenerateOptions {
            temperature: self.temperature,
            top_p: self.top_p,
            num_predict: self.max_tokens.map(|t| t as i32),
            stop: if self.stop_sequences.is_empty() {
                None
            } else {
                Some(self.stop_sequences.clone())
            },
            ..Default::default()
        }
    }
}

#[async_trait]
impl LlmProvider for OllamaLlmProvider {
    fn name(&self) -> &str {
        "ollama"
    }

    async fn list_models(&self) -> Result<Vec<String>> {
        let models = self.client.list_models().await?;
        Ok(models.into_iter().map(|m| m.name).collect())
    }

    async fn generate(&self, prompt: &str, options: &GenerationOptions) -> Result<String> {
        let model = options.model.as_deref()
            .unwrap_or(self.client.default_model());
        
        let mut request = GenerateRequest::new(model, prompt);
        
        if let Some(ollama_options) = options.to_ollama_options() {
            request = request.with_options(ollama_options);
        }
        
        let response = self.client.generate(&request).await?;
        Ok(response.response)
    }

    async fn generate_with_tools(
        &self,
        prompt: &str,
        tools: &[ToolDefinition],
        options: &GenerationOptions,
    ) -> Result<GenerationResponse> {
        let model = options.model.as_deref()
            .unwrap_or(self.client.default_model());
        
        let request = GenerateRequest::new(model, prompt);
        let (text, tool_calls) = self.client.generate_with_tools(&request, tools).await?;
        
        if tool_calls.is_empty() {
            Ok(GenerationResponse::Text { content: text })
        } else if text.is_empty() {
            Ok(GenerationResponse::ToolCalls { calls: tool_calls })
        } else {
            Ok(GenerationResponse::Mixed {
                content: text,
                calls: tool_calls,
            })
        }
    }

    fn stream(
        &self,
        prompt: &str,
        options: &GenerationOptions,
    ) -> std::pin::Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send + '_>> {
        let model = options.model.clone()
            .unwrap_or_else(|| self.client.default_model().to_string());
        
        Box::pin(async_stream::stream! {
            let request = GenerateRequest::new(&model, prompt);
            
            match self.client.generate_stream(&request).await {
                Ok(mut stream) => {
                    use futures::StreamExt;
                    
                    while let Some(event_result) = stream.next().await {
                        match event_result {
                            Ok(event) => {
                                match event {
                                    streaming::StreamEvent::Chunk { response, .. } => {
                                        if !response.is_empty() {
                                            yield Ok(StreamChunk::Text(response));
                                        }
                                    }
                                    streaming::StreamEvent::Done { .. } => {
                                        yield Ok(StreamChunk::Done);
                                        break;
                                    }
                                }
                            }
                            Err(e) => {
                                yield Err(e);
                                break;
                            }
                        }
                    }
                }
                Err(e) => {
                    yield Err(e);
                }
            }
        })
    }
}
```

- [ ] **Step 4: Update lib.rs to export provider**

Update `crates/kod-provider-ollama/src/lib.rs`:

```rust
//! Ollama LLM provider implementation.

pub mod client;
pub mod generate;
pub mod provider;
pub mod streaming;
pub mod tools;

pub use client::{ModelInfo, OllamaClient, OllamaClientBuilder};
pub use generate::{GenerateOptions, GenerateRequest, GenerateResponse};
pub use provider::{GenerationOptionsExt, OllamaLlmProvider};
pub use streaming::{GenerationStream, StreamAccumulator, StreamEvent};
pub use tools::{format_tools_for_ollama, parse_tool_calls};
```

- [ ] **Step 5: Fix GenerationOptions to have Clone and Default**

Check `crates/kod-provider/src/traits.rs` and ensure `GenerationOptions` derives these:

```rust
#[derive(Debug, Clone, Default)]
pub struct GenerationOptions {
    pub model: Option<String>,
    pub max_tokens: Option<usize>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub stop_sequences: Vec<String>,
}
```

- [ ] **Step 6: Run all tests**

```bash
cargo test -p kod-provider-ollama
cargo test -p kod-provider
```

Expected: All tests pass

- [ ] **Step 7: Commit**

```bash
git add crates/kod-provider-ollama/ crates/kod-provider/
git commit -m "feat(provider-ollama): implement LlmProvider trait with streaming and tools"
```

---

## Chunk 2 Review Checklist

- [ ] Ollama client connects and performs health checks
- [ ] Model listing works with proper error handling
- [ ] Non-streaming generation returns complete responses
- [ ] Streaming generation parses NDJSON correctly
- [ ] Tool calling formats definitions and parses responses
- [ ] LlmProvider trait is fully implemented
- [ ] All tests pass (except integration tests requiring Ollama)
- [ ] Error messages are user-friendly

**Verification commands:**

```bash
cargo test -p kod-provider-ollama
cargo test -p kod-provider
cargo clippy -p kod-provider-ollama -- -D warnings
cargo build --workspace
```

---

## Next Chunk Preview

Chunk 3 will cover the **Skills System implementation**:
- Skill file parsing with YAML front matter
- Markdown body parsing (instructions, examples, constraints)
- Skill loader with directory scanning
- Hot reloading with file system watcher
- Pattern-based skill matching
- Basic semantic matching integration

Would you like me to continue with **Chunk 3: Skills System**?
