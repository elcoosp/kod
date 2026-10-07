//! Probe: does the mock's POST route work when the request is made
//! from the same process (no PTY, no TUI)?
//!
//! If this passes but the TUI's POST never arrives, the bug is on
//! the TUI side. If it fails, the mock's routing is broken.

use std::time::Duration;

#[test]
fn mock_handles_get_and_post() {
    let mock = super::mock_llm::MockServer::start(super::mock_llm::MOCK_REPLY);
    let base = format!("http://127.0.0.1:{}/v1", mock.port);
    eprintln!("probe: mock on {base}");

    // Build a one-thread runtime so the requests run to completion.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();

        // GET /v1/models
        let r = client
            .get(format!("{base}/models"))
            .send()
            .await
            .expect("GET /v1/models");
        eprintln!("probe: GET status = {}", r.status());
        assert!(r.status().is_success());

        // POST /v1/chat/completions — non-streaming variant first to
        // see whether the request even reaches the router.
        let body = serde_json::json!({
            "model": "mock-model",
            "messages": [{"role": "user", "content": "hi"}],
            "stream": false,
        });
        let r2 = client
            .post(format!("{base}/chat/completions"))
            .json(&body)
            .send()
            .await
            .expect("POST /v1/chat/completions");
        eprintln!("probe: POST status = {}", r2.status());
        let text = r2.text().await.unwrap_or_default();
        eprintln!("probe: POST body = {text}");
        assert!(text.contains("data:"), "POST body missing SSE: {text}");
    });
}
