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
            .map_err(|e| KodError::Config(format!("could not build embedder http client: {e}")))?;
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
                .map_err(|e| KodError::Network(format!("ollama embedder: POST {url}: {e}")))?;
            let status = resp.status();
            if !status.is_success() {
                let text = resp.text().await.unwrap_or_default();
                return Err(KodError::Provider(format!(
                    "ollama embedder: {url} returned {status}: {text}"
                )));
            }
            let parsed: serde_json::Value = resp
                .json()
                .await
                .map_err(|e| KodError::Provider(format!("ollama embedder: bad JSON: {e}")))?;
            let embeddings = parsed
                .get("embeddings")
                .and_then(|v| v.as_array())
                .ok_or_else(|| {
                    KodError::Provider("ollama embedder: response missing 'embeddings'".to_string())
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
            .map_err(|e| KodError::Config(format!("could not build embedder http client: {e}")))?;
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
                .map_err(|e| KodError::Network(format!("openai embedder: POST {url}: {e}")))?;
            let status = resp.status();
            if !status.is_success() {
                let text = resp.text().await.unwrap_or_default();
                return Err(KodError::Provider(format!(
                    "openai embedder: {url} returned {status}: {text}"
                )));
            }
            let parsed: serde_json::Value = resp
                .json()
                .await
                .map_err(|e| KodError::Provider(format!("openai embedder: bad JSON: {e}")))?;
            let data = parsed
                .get("data")
                .and_then(|v| v.as_array())
                .ok_or_else(|| {
                    KodError::Provider("openai embedder: response missing 'data'".to_string())
                })?;
            for entry in data {
                let emb = entry.get("embedding").ok_or_else(|| {
                    KodError::Provider("openai embedder: entry missing 'embedding'".to_string())
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

/// Build an embedder from the memory config (design D2.1).
///
/// `memory.embedding_endpoint` selects the wire shape; the other three
/// fields carry the model name, an optional explicit URL, and an
/// optional environment variable holding the API key.
///
/// `llm_base_url` is only consulted for the Ollama case when
/// `embedding_url` is unset: Ollama's embed endpoint shares the
/// server root with the chat endpoint, and the user almost always
/// configures one and not the other. Passing `None` leaves the URL
/// unresolved, which is an error the caller sees on the first
/// `embed()` call rather than at construction — the same "fail
/// loudly when it matters" shape every other client uses.
///
/// Returns `None` when the endpoint is `None` (the default) or when
/// an embedder could not be built. A caller that gets `None` keeps
/// the keyword+recency fallback the D2.3 scorer provides; no
/// retrieval path is broken by an absent embedder.
///
/// The function is intentionally infallible: a misconfiguration
/// (an OpenAI endpoint with no API key in the environment) logs a
/// warning and returns `None`. A caller does not have to thread a
/// `Result` through its own setup for a subsystem whose absence is
/// already a supported mode.
pub fn from_config(
    memory: &kod_config::MemoryConfig,
    llm_base_url: Option<&str>,
) -> Option<std::sync::Arc<dyn EmbeddingClient>> {
    use kod_config::EmbeddingEndpoint;

    match memory.embedding_endpoint {
        EmbeddingEndpoint::None => None,
        EmbeddingEndpoint::Ollama => {
            let url = memory
                .embedding_url
                .clone()
                .or_else(|| llm_base_url.map(derive_ollama_root))
                .unwrap_or_else(|| "http://localhost:11434".to_string());
            match OllamaEmbedder::new(url.clone(), memory.embedding_model.clone()) {
                Ok(e) => Some(std::sync::Arc::new(e)),
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        url = %url,
                        "could not build Ollama embedder; semantic scoring disabled",
                    );
                    None
                }
            }
        }
        EmbeddingEndpoint::OpenAI => {
            let url = memory
                .embedding_url
                .clone()
                .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
            // API key: the named env var first, then `OPENAI_API_KEY`.
            // A missing key is a warning, not a hard error: an
            // operator who set `embedding_endpoint = "openai"` and
            // forgot the key still gets a working session — just with
            // keyword retrieval — and a log line naming the miss.
            let key = memory
                .embedding_api_key_env
                .as_deref()
                .and_then(|var| std::env::var(var).ok())
                .or_else(|| std::env::var("OPENAI_API_KEY").ok())
                .unwrap_or_default();
            if key.is_empty() {
                tracing::warn!(
                    endpoint = "openai",
                    var = memory
                        .embedding_api_key_env
                        .as_deref()
                        .unwrap_or("OPENAI_API_KEY"),
                    "no OpenAI API key in the environment; semantic scoring disabled",
                );
                return None;
            }
            match OpenAIEmbedder::new(url.clone(), memory.embedding_model.clone(), key) {
                Ok(e) => Some(std::sync::Arc::new(e)),
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        url = %url,
                        "could not build OpenAI embedder; semantic scoring disabled",
                    );
                    None
                }
            }
        }
    }
}

/// Turn an OpenAI-compatible chat base URL (`http://host:port/v1`,
/// `http://host:port`, or a full URL with a path) into the Ollama
/// server root that hosts `/api/embed`.
///
/// The three shapes a user's config can carry:
///
/// - `http://localhost:11434/v1` → `http://localhost:11434`
/// - `http://localhost:11434`    → unchanged
/// - anything else               → unchanged (a proxy that fronts both
///   `/v1/chat/completions` and `/api/embed` gets the path preserved)
///
/// Trailing slashes are trimmed so the eventual
/// `format!("{root}/api/embed")` does not produce a double slash.
fn derive_ollama_root(base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    match trimmed.strip_suffix("/v1") {
        Some(root) => root.to_string(),
        None => trimmed.to_string(),
    }
}

/// Parse a JSON array of numbers into `Vec<f32>`. Rejects non-array
/// shapes and entries that are not numbers; truncates absurdly long
/// arrays (defensive — a server returning 10⁶ floats is a bug).
fn parse_float_array(v: &serde_json::Value) -> Result<Vec<f32>> {
    const MAX_DIMS: usize = 8192;
    let arr = v
        .as_array()
        .ok_or_else(|| KodError::Provider("embedding: expected an array of numbers".to_string()))?;
    if arr.len() > MAX_DIMS {
        return Err(KodError::Provider(format!(
            "embedding: {} dims exceeds the {MAX_DIMS} cap",
            arr.len()
        )));
    }
    let mut out = Vec::with_capacity(arr.len());
    for x in arr {
        let f = x
            .as_f64()
            .ok_or_else(|| KodError::Provider(format!("embedding: non-numeric entry {x}")))?;
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
        assert!(msg.contains("no embedding endpoint"), "got: {msg}");
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
        let e = OllamaEmbedder::new("http://localhost:11434/", "nomic-embed-text").unwrap();
        // Fields are private; assert via name() as a light smoke test.
        assert_eq!(e.name(), "ollama");
    }

    #[test]
    fn from_config_none_returns_none() {
        let cfg = kod_config::MemoryConfig::default();
        // Default `embedding_endpoint` is `None`; no embedder.
        assert!(from_config(&cfg, None).is_none());
    }

    #[test]
    fn from_config_ollama_derives_url_from_llm_base() {
        let cfg = kod_config::MemoryConfig {
            embedding_endpoint: kod_config::EmbeddingEndpoint::Ollama,
            embedding_model: "nomic-embed-text".to_string(),
            ..Default::default()
        };
        let embedder = from_config(&cfg, Some("http://localhost:11434/v1"));
        let embedder = embedder.expect("ollama embedder should build");
        assert_eq!(embedder.name(), "ollama");
    }

    #[test]
    fn from_config_openai_without_key_returns_none() {
        // Ensure no leaked env var turns this into an accidental success.
        // SAFETY: no other test in this file reads OPENAI_API_KEY.
        let prior = std::env::var("OPENAI_API_KEY").ok();
        // SAFETY: single-threaded test; the removal is restored below.
        unsafe { std::env::remove_var("OPENAI_API_KEY") };

        let cfg = kod_config::MemoryConfig {
            embedding_endpoint: kod_config::EmbeddingEndpoint::OpenAI,
            ..Default::default()
        };
        let embedder = from_config(&cfg, None);
        assert!(embedder.is_none(), "missing API key must yield None");

        // Restore the environment for the rest of the test binary.
        if let Some(v) = prior {
            // SAFETY: same reasoning.
            unsafe { std::env::set_var("OPENAI_API_KEY", v) };
        }
    }

    #[test]
    fn derive_ollama_root_strips_v1_suffix() {
        assert_eq!(
            derive_ollama_root("http://localhost:11434/v1"),
            "http://localhost:11434"
        );
        assert_eq!(
            derive_ollama_root("http://localhost:11434/v1/"),
            "http://localhost:11434"
        );
        assert_eq!(
            derive_ollama_root("http://localhost:11434"),
            "http://localhost:11434"
        );
        // A path-carrying URL is left alone (proxy case).
        assert_eq!(
            derive_ollama_root("https://proxy.example/ollama"),
            "https://proxy.example/ollama"
        );
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
