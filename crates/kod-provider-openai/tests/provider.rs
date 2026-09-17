//! OpenAI-compatible provider tests.
//!
//! `from_config` was removed in Cleanup-3a: the `LlmConfig` v2 shape
//! carries a list of `EndpointConfig` entries rather than a single
//! model / base_url pair, and every provider is built by
//! `kod_core::build_registry` or by a direct `with_api_key` call.
//! These tests exercise the constructor and endpoint-normalisation
//! paths that remain.

use kod_provider::LlmProvider;
use kod_provider_openai::{OpenAICompatProvider, normalize_base_url};

#[test]
fn test_normalize_base_url() {
    // Server root gets /v1 appended.
    assert_eq!(
        normalize_base_url("http://localhost:11434"),
        "http://localhost:11434/v1"
    );
    assert_eq!(
        normalize_base_url("http://localhost:1234"),
        "http://localhost:1234/v1"
    );
    // Already-normalised roots are preserved.
    assert_eq!(
        normalize_base_url("http://localhost:1234/v1"),
        "http://localhost:1234/v1"
    );
    assert_eq!(
        normalize_base_url("http://localhost:11434/v1/"),
        "http://localhost:11434/v1"
    );
    assert_eq!(
        normalize_base_url("https://api.openai.com/v1"),
        "https://api.openai.com/v1"
    );
}

#[test]
fn test_new_normalizes_and_keeps_model() {
    let provider = OpenAICompatProvider::new("http://localhost:11434", "llama3.1").unwrap();
    assert_eq!(provider.base_url(), "http://localhost:11434/v1");
    assert_eq!(provider.default_model(), "llama3.1");

    let provider =
        OpenAICompatProvider::new("http://localhost:1234/v1", "local-model").unwrap();
    assert_eq!(provider.base_url(), "http://localhost:1234/v1");
    assert_eq!(provider.default_model(), "local-model");
}

#[test]
fn test_with_model_keeps_endpoint() {
    let provider = OpenAICompatProvider::new("http://localhost:1234/v1", "model-a")
        .unwrap()
        .with_model("model-b")
        .unwrap();
    assert_eq!(provider.default_model(), "model-b");
    assert_eq!(provider.base_url(), "http://localhost:1234/v1");
}

#[test]
fn test_provider_name_is_stable() {
    let provider =
        OpenAICompatProvider::new("http://localhost:11434/v1", "x").unwrap();
    assert_eq!(provider.name(), "openai-compatible");
}
