use kod_provider_ollama::OllamaClient;

#[tokio::test]
async fn test_client_creation() {
    let client = OllamaClient::new("http://localhost:11434");
    assert_eq!(client.base_url(), "http://localhost:11434");
}

#[tokio::test]
async fn test_client_creation_trailing_slash() {
    let client = OllamaClient::new("http://localhost:11434/");
    assert_eq!(client.base_url(), "http://localhost:11434");
}

#[tokio::test]
async fn test_client_with_custom_model() {
    let client = OllamaClient::new("http://localhost:11434").with_model("llama3.2");
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

#[tokio::test]
#[ignore = "requires running Ollama server"]
async fn test_health_check_unreachable() {
    let client = OllamaClient::new("http://localhost:19999"); // Non-existent port
    let result = client.health_check().await;
    assert!(result.is_err());
}
