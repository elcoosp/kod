use kod_provider_ollama::streaming::{parse_stream_line, StreamEvent, StreamAccumulator};

#[test]
fn test_stream_event_parsing() {
    // Simulate Ollama's newline-delimited JSON responses
    let json = r#"{"model":"llama3.2","response":"Hello","done":false}"#;
    let event = parse_stream_line(json).unwrap();

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
    let event = parse_stream_line(json).unwrap();

    match event {
        StreamEvent::Done { total_duration, eval_count, .. } => {
            assert_eq!(total_duration, 1_000_000_000);
            assert_eq!(eval_count, 10);
        }
        _ => panic!("Expected done event"),
    }
}

#[tokio::test]
async fn test_stream_accumulation() {
    let mut accumulator = StreamAccumulator::new();

    // Simulate streaming chunks
    let chunks = vec!["Hello", " ", "world", "!"];
    for chunk in chunks {
        accumulator.add_chunk(chunk).unwrap();
    }

    let result = accumulator.finish().unwrap();
    assert_eq!(result, "Hello world!");
}

#[tokio::test]
async fn test_accumulator_finish_with_stats() {
    let mut accumulator = StreamAccumulator::new();
    accumulator.add_chunk("Hello").unwrap();
    accumulator.add_chunk(" world").unwrap();
    accumulator.finish_with_stats(1_000_000_000, 10).unwrap();

    assert!(accumulator.is_done());
    let tps = accumulator.tokens_per_second().unwrap();
    assert!((tps - 10.0).abs() < 0.1); // 10 tokens in 1 second
}

#[tokio::test]
async fn test_accumulator_rejects_after_done() {
    let mut accumulator = StreamAccumulator::new();
    accumulator.add_chunk("Hello").unwrap();
    accumulator.finish().unwrap();

    let result = accumulator.add_chunk("world");
    assert!(result.is_err());
}
