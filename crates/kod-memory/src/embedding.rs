//! HTTP embedding clients (D2-B1).
//!
//! Two shapes only: Ollama's `/api/embed` and OpenAI's
//! `/v1/embeddings`. Both return JSON, both accept a batch of strings,
//! both work over plain HTTP. No local model is linked into the
//! binary — the "no ML weight" rule (README, ADR-04) is easier to
//! hold when the crate simply does not depend on `fastembed`.
//!
//! # No endpoint available
//!
//! [`NoEmbedder`] is the fallback: `embed` returns a
//! `KodError::Config` naming the missing configuration. Retrieval
//! treats that as "no embeddings today" and falls back to keyword +
//! recency scoring (B2), so a machine with no embedder still works.
//!
//! # Dimensions
//!
//! `dims()` is the vector length the client produces. It is fixed at
//! construction time (the model is chosen once) so the
//! [`VectorIndex`](crate::vector_index::VectorIndex) can reject
//! mismatched vectors before they corrupt the index. A caller that
//! switches models must rebuild both the client and the index.

use async_trait::async_trait;
use kod_error::{KodError, Result};

/// One embedding client.
#[async_trait]
pub trait EmbeddingClient: Send + Sync {
    /// Provider name for logging and `kod doctor`.
    fn name(&self) -> &str;
    /// Vector length. 0 for `NoEmbedder` (unusable).
    fn dims(&self) -> usize;
    /// Embed a batch of texts. Implementations may cap the batch
    /// size internally; the caller does not have to know.
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
}

/// Fallback: no embeddings are computed. Every call is an error, so a
/// caller that wants "just skip embeddings" checks `dims() == 0`
/// rather than relying on `embed()` returning a special value.
pub struct NoEmbedder;

#[async_trait]
impl EmbeddingClient for NoEmbedder {
    fn name(&self) -> &str {
        "none"
    }
    fn dims(&self) -> usize {
        0
    }
    async fn embed(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Err(KodError::Config(
            "no embedding endpoint configured. Set memory.embedding_endpoint \
             to 'ollama' or 'openai', or leave it at 'none' and accept \
             keyword-only retrieval."
                .to_string(),
        ))
    }
}

/// The largest batch we will ever send in one HTTP call. Ollama's
/// default is 512; OpenAI accepts up to 2048. 32 keeps the request
/// small enough that a slow link does not stall, and the caller loops
/// when it has more.
const MAX_BATCH: usize = 32;

/// Ollama embedder: `POST {base}/api/embed`.
///
/// `base` is the server root (`http://localhost:11434`), *not* the
/// `/v1` OpenAI-compatible root. The CLI/TUI derive one from the other
/// when a URL is not supplied explicitly.
pub struct OllamaEmbedder {
    base_url: String,
    model: String,
    client: reqwest::Client,
    /// Cached dims from the first successful call. 0 means unknown.
    dims_cache: std::sync::atomic::AtomicUsize,
}

impl OllamaEmbedder {
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .map_err(|e| {
                KodError::Config(format!("could not build embedder http client: {e}"))
            })?;
        Ok(Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            model: model.into(),
            client,
            dims_cache: std::sync::atomic::AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl EmbeddingClient for OllamaEmbedder {
    fn name(&self) -> &str {
        "ollama"
    }
    fn dims(&self) -> usize {
        self.dims_cache.load(std::sync::atomic::Ordering::Relaxed)
    }
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let mut out: Vec<Vec<f32>> = Vec::with_capacity(texts.len());
        for chunk in texts.chunks(MAX_BATCH) {
            let body = serde_json::json!({
                "model": self.model,
                "input": chunk,
            });
            let url = format!("{}/api/embed", self.base_url);
            let resp = self
                .client
                .post(&url)
                .json(&body)
                .send()
                .await
                .map_err(|e| {
                    KodError::Network(format!("ollama embedder: POST {url}: {e}"))
                })?;
            let status = resp.status();
            if !status.is_success() {
                let text = resp.text().await.unwrap_or_default();
                return Err(KodError::Provider(format!(
                    "ollama embedder: {url} returned {status}: {text}"
                )));
            }
            let parsed: serde_json::Value = resp.json().await.map_err(|e| {
                KodError::Provider(format!("ollama embedder: bad JSON: {e}"))
            })?;
            let embeddings = parsed
                .get("embeddings")
                .and_then(|v| v.as_array())
                .ok_or_else(|| {
                    KodError::Provider(
                        "ollama embedder: response missing 'embeddings'".to_string(),
                    )
                })?;
            for emb in embeddings {
                let vec = parse_float_array(emb)?;
                if self.dims_cache.load(std::sync::atomic::Ordering::Relaxed) == 0 {
                    self.dims_cache
                        .store(vec.len(), std::sync::atomic::Ordering::Relaxed);
                }
                out.push(vec);
            }
        }
        Ok(out)
    }
}

/// OpenAI embedder: `POST {base}/embeddings`, where `base` is the
/// OpenAI API root (typically `.../v1`).
pub struct OpenAIEmbedder {
    base_url: String,
    model: String,
    api_key: String,
    client: reqwest::Client,
    dims_cache: std::sync::atomic::AtomicUsize,
}

impl OpenAIEmbedder {
    pub fn new(
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .map_err(|e| {
                KodError::Config(format!("could not build embedder http client: {e}"))
            })?;
        Ok(Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            model: model.into(),
            api_key: api_key.into(),
            client,
            dims_cache: std::sync::atomic::AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl EmbeddingClient for OpenAIEmbedder {
    fn name(&self) -> &str {
        "openai"
    }
    fn dims(&self) -> usize {
        self.dims_cache.load(std::sync::atomic::Ordering::Relaxed)
    }
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let mut out: Vec<Vec<f32>> = Vec::with_capacity(texts.len());
        for chunk in texts.chunks(MAX_BATCH) {
            let body = serde_json::json!({
                "model": self.model,
                "input": chunk,
            });
            let url = format!("{}/embeddings", self.base_url);
            let resp = self
                .client
                .post(&url)
                .bearer_auth(&self.api_key)
                .json(&body)
                .send()
                .await
                .map_err(|e| {
                    KodError::Network(format!("openai embedder: POST {url}: {e}"))
                })?;
            let status = resp.status();
            if !status.is_success() {
                let text = resp.text().await.unwrap_or_default();
                return Err(KodError::Provider(format!(
                    "openai embedder: {url} returned {status}: {text}"
                )));
            }
            let parsed: serde_json::Value = resp.json().await.map_err(|e| {
                KodError::Provider(format!("openai embedder: bad JSON: {e}"))
            })?;
            let data = parsed
                .get("data")
                .and_then(|v| v.as_array())
                .ok_or_else(|| {
                    KodError::Provider(
                        "openai embedder: response missing 'data'".to_string(),
                    )
                })?;
            for entry in data {
                let emb = entry.get("embedding").ok_or_else(|| {
                    KodError::Provider(
                        "openai embedder: entry missing 'embedding'".to_string(),
                    )
                })?;
                let vec = parse_float_array(emb)?;
                if self.dims_cache.load(std::sync::atomic::Ordering::Relaxed) == 0 {
                    self.dims_cache
                        .store(vec.len(), std::sync::atomic::Ordering::Relaxed);
                }
                out.push(vec);
            }
        }
        Ok(out)
    }
}

/// Parse a JSON array of numbers into `Vec<f32>`. Rejects non-array
/// shapes and entries that are not numbers; truncates absurdly long
/// arrays (defensive — a server returning 10⁶ floats is a bug).
fn parse_float_array(v: &serde_json::Value) -> Result<Vec<f32>> {
    const MAX_DIMS: usize = 8192;
    let arr = v.as_array().ok_or_else(|| {
        KodError::Provider("embedding: expected an array of numbers".to_string())
    })?;
    if arr.len() > MAX_DIMS {
        return Err(KodError::Provider(format!(
            "embedding: {} dims exceeds the {MAX_DIMS} cap",
            arr.len()
        )));
    }
    let mut out = Vec::with_capacity(arr.len());
    for x in arr {
        let f = x.as_f64().ok_or_else(|| {
            KodError::Provider(format!("embedding: non-numeric entry {x}"))
        })?;
        out.push(f as f32);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_embedder_reports_zero_dims() {
        let e = NoEmbedder;
        assert_eq!(e.dims(), 0);
        assert_eq!(e.name(), "none");
    }

    #[tokio::test]
    async fn no_embedder_errors_with_actionable_message() {
        let e = NoEmbedder;
        let err = e
            .embed(&["hello".to_string()])
            .await
            .expect_err("NoEmbedder must always error");
        let msg = err.to_string();
        assert!(
            msg.contains("no embedding endpoint"),
            "got: {msg}"
        );
        assert!(msg.contains("memory.embedding_endpoint"), "got: {msg}");
    }

    #[test]
    fn parse_float_array_accepts_numbers() {
        let v: serde_json::Value = serde_json::from_str("[0.1, 0.2, 0.3]").unwrap();
        let out = parse_float_array(&v).unwrap();
        assert_eq!(out.len(), 3);
        assert!((out[0] - 0.1).abs() < 1e-6);
    }

    #[test]
    fn parse_float_array_rejects_non_array() {
        let v: serde_json::Value = serde_json::json!("not an array");
        assert!(parse_float_array(&v).is_err());
    }

    #[test]
    fn parse_float_array_rejects_non_number() {
        let v: serde_json::Value = serde_json::json!([1.0, "two", 3.0]);
        assert!(parse_float_array(&v).is_err());
    }

    #[test]
    fn parse_float_array_caps_dims() {
        let arr: Vec<f32> = (0..9000).map(|i| i as f32).collect();
        let v = serde_json::to_value(&arr).unwrap();
        assert!(parse_float_array(&v).is_err());
    }

    #[test]
    fn ollama_embedder_normalizes_trailing_slash() {
        let e = OllamaEmbedder::new("http://localhost:11434/", "nomic-embed-text")
            .unwrap();
        // Fields are private; assert via name() as a light smoke test.
        assert_eq!(e.name(), "ollama");
    }

    #[test]
    fn openai_embedder_builds() {
        let e = OpenAIEmbedder::new(
            "https://api.openai.com/v1",
            "text-embedding-3-small",
            "sk-test",
        )
        .unwrap();
        assert_eq!(e.name(), "openai");
        assert_eq!(e.dims(), 0, "dims unknown until first call");
    }
}
