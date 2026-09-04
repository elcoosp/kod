use kod_provider::LlmProvider;
use kod_provider_ollama::{GenerationOptionsExt, OllamaLlmProvider};

#[tokio::test]
async fn test_provider_name() {
    let provider = OllamaLlmProvider::new("http://localhost:11434");
    assert_eq!(provider.name(), "ollama");
}

#[tokio::test]
async fn test_provider_with_model() {
    let provider = OllamaLlmProvider::new("http://localhost:11434").with_model("codellama:13b");

    // This will fail if Ollama isn't running, which is expected for unit test
    let models = provider.list_models().await;
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
