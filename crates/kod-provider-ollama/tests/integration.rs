use kod_provider_ollama::{GenerateRequest, OllamaClient};
use kod_types::{ToolCategory, ToolDefinition, ToolId, ToolPermissions};
use serde_json::json;

#[tokio::test]
#[ignore = "requires running Ollama server"]
async fn test_real_generation() {
    let client = OllamaClient::new("http://localhost:11434");

    // First check health
    client
        .health_check()
        .await
        .expect("Ollama should be running");

    // List models
    let models = client.list_models().await.unwrap();
    assert!(!models.is_empty(), "Should have at least one model");

    // Use first available model
    let model_name = models[0].name.clone();

    // Generate
    let request =
        GenerateRequest::new(&model_name, "Say 'hello' and nothing else.").with_temperature(0.1);

    let response = client.generate(&request).await.unwrap();
    assert!(response.done);
    assert!(!response.response.is_empty());
    assert!(response.tokens_per_second() > 0.0);
}

#[tokio::test]
#[ignore = "requires running Ollama server"]
async fn test_real_streaming() {
    use futures::StreamExt;
    use kod_provider_ollama::StreamAccumulator;

    let client = OllamaClient::new("http://localhost:11434");
    client
        .health_check()
        .await
        .expect("Ollama should be running");

    let models = client.list_models().await.unwrap();
    let model_name = models[0].name.clone();

    let request = GenerateRequest::new(&model_name, "Count from 1 to 5.");

    let mut stream = client.generate_stream(&request).await.unwrap();
    let mut accumulator = StreamAccumulator::new();

    while let Some(event_result) = stream.next().await {
        let event = event_result.unwrap();
        match event {
            kod_provider_ollama::StreamEvent::Chunk { response, .. } => {
                accumulator.add_chunk(&response).unwrap();
            }
            kod_provider_ollama::StreamEvent::Done {
                total_duration,
                eval_count,
                ..
            } => {
                accumulator
                    .finish_with_stats(total_duration, eval_count)
                    .unwrap();
            }
        }
    }

    let final_text = accumulator.finish().unwrap();
    assert!(!final_text.is_empty());
    assert!(final_text.contains('1'));
}

#[tokio::test]
#[ignore = "requires running Ollama with tool-supporting model"]
async fn test_tool_calling() {
    let client = OllamaClient::new("http://localhost:11434");
    client
        .health_check()
        .await
        .expect("Ollama should be running");

    let request = GenerateRequest::new("llama3.2", "What is the weather in Paris?");

    let tools = vec![ToolDefinition {
        id: ToolId::new(),
        name: "get_weather".to_string(),
        description: "Get current weather for a location".to_string(),
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
    }];

    let (text, tool_calls) = client.generate_with_tools(&request, &tools).await.unwrap();

    // Model may or may not call the tool, but should not error
    println!("Text: {}", text);
    println!("Tool calls: {:?}", tool_calls);
}
