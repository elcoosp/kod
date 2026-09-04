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
