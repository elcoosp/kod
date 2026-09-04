//! Ollama HTTP client.
//!
//! Handles communication with the Ollama API server.

/// Client for communicating with the Ollama API server.
// TODO: Implement full client in Chunk 2
#[allow(dead_code)]
pub struct OllamaClient {
    base_url: String,
}

impl OllamaClient {
    pub fn new(base_url: String) -> Self {
        Self { base_url }
    }
}
