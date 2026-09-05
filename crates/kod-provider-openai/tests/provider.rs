use kod_config::LlmConfig;
use kod_provider::{GenerationOptions, LlmProvider};
use kod_provider_openai::{OpenAICompatProvider, normalize_base_url};

#[test]
fn test_provider_name() {
    let provider = OpenAICompatProvider::new("http://localhost:11434", "llama3.1").unwrap();
    assert_eq!(provider.name(), "openai-compatible");
}

#[test]
fn test_normalize_base_url() {
    // Bare server roots (Ollama native, LM Studio without /v1) gain the suffix.
    assert_eq!(
        normalize_base_url("http://localhost:11434"),
        "http://localhost:11434/v1"
    );
    assert_eq!(
        normalize_base_url("http://localhost:1234"),
        "http://localhost:1234/v1"
    );
    // Existing API roots are untouched, including trailing slashes.
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
fn test_new_normalizes_endpoint() {
    let provider = OpenAICompatProvider::new("http://localhost:11434", "llama3.1").unwrap();
    assert_eq!(provider.base_url(), "http://localhost:11434/v1");
    assert_eq!(provider.default_model(), "llama3.1");

    let provider = OpenAICompatProvider::new("http://localhost:1234/v1", "local-model").unwrap();
    assert_eq!(provider.base_url(), "http://localhost:1234/v1");
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
fn test_from_config_uses_override() {
    let config = LlmConfig {
        base_url: "http://localhost:1234".to_string(),
        model: "config-model".to_string(),
        ..Default::default()
    };

    let provider = OpenAICompatProvider::from_config(&config, None).unwrap();
    assert_eq!(provider.default_model(), "config-model");
    assert_eq!(provider.base_url(), "http://localhost:1234/v1");

    let provider = OpenAICompatProvider::from_config(&config, Some("flag-model")).unwrap();
    assert_eq!(provider.default_model(), "flag-model");
}

#[tokio::test]
#[ignore = "requires a running OpenAI-compatible server (Ollama, LM Studio, MLX, ...).
  Set KOD_TEST_MODEL to a model the server has, e.g.
  KOD_TEST_MODEL=qwen3:0.6b cargo test -p kod-provider-openai -- --ignored"]
async fn test_live_list_models() {
    let model = std::env::var("KOD_TEST_MODEL").unwrap_or_else(|_| "llama3.1".to_string());
    let provider = OpenAICompatProvider::new("http://localhost:11434", &model).unwrap();

    // list_models: GET {base_url}/models (OpenAI spec).
    let models = provider
        .list_models()
        .await
        .expect("model listing must succeed");
    assert!(!models.is_empty(), "server should advertise models");

    // generate: POST {base_url}/chat/completions, non-streaming.
    let options = GenerationOptions {
        max_tokens: Some(50),
        ..Default::default()
    };
    let reply = provider
        .generate("Reply with exactly: LIVE_OK", &options)
        .await
        .expect("completion must succeed");
    assert!(reply.contains("LIVE_OK"), "unexpected reply: {reply}");

    // stream: same endpoint with stream=true, chunks must carry text.
    use futures::StreamExt;
    use kod_provider::StreamChunk;
    let mut chunks = provider.stream("Reply with exactly: STREAM_OK", &options);
    let mut text = String::new();
    let mut done = false;
    while let Some(chunk) = chunks.next().await {
        match chunk.expect("chunk must be Ok") {
            StreamChunk::Text(t) => text.push_str(&t),
            StreamChunk::Done => {
                done = true;
                break;
            }
            _ => {}
        }
    }
    assert!(done, "stream must terminate with Done");
    assert!(
        text.contains("STREAM_OK"),
        "unexpected streamed reply: {text}"
    );
}
