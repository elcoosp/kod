//! Malformed JSON-RPC from a real MCP server (session 18, fault-injection arc).
//!
//! The `mcp_line` fuzz target proves the line *parser* is hardened
//! (`fuzz/fuzz_targets/mcp_line.rs`, using `parse_line_for_fuzz`). It
//! does not prove the *client* recovers from a garbage line in the
//! middle of a live conversation: that the reader logs and skips it,
//! that the pending-request correlation survives, that a subsequent
//! valid response still reaches its caller. This test does.
//!
//! The second test pins a related property: a request that *times
//! out* (the server never responds) must not poison the client. The
//! `request()` timeout path clears its `pending[id]` (client.rs:400),
//! so a later request on the same client can still succeed — this
//! test proves that end to end.
//!
//! Both use a tiny Python fake server, matching the template in
//! `client.rs::tests::FAKE_SERVER`. Skips gracefully when `python3`
//! is absent.

use kod_mcp::McpClient;
use std::collections::BTreeMap;
use std::time::Duration;
use tempfile::TempDir;

fn python_available() -> bool {
    std::process::Command::new("python3")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

async fn spawn_with_script(script: &str) -> (TempDir, McpClient) {
    let dir = TempDir::new().expect("tempdir");
    std::fs::write(dir.path().join("server.py"), script).expect("write server");
    let path = dir.path().join("server.py");
    let client = McpClient::spawn_stdio(
        "python3",
        &[path.to_string_lossy().to_string()],
        &BTreeMap::new(),
    )
    .await
    .expect("spawn fake server");
    (dir, client)
}

/// Emits one garbage line immediately before every `tools/call`
/// response. A robust reader logs and skips it; the real response
/// that follows must still match its pending id.
const GARBAGE_BEFORE_VALID: &str = r#"import json, sys
def emit(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    method = msg.get("method")
    mid = msg.get("id")
    if method == "initialize":
        emit({"jsonrpc":"2.0","id":mid,"result":{
            "protocolVersion":"2025-06-18",
            "capabilities":{"tools":{}},
            "serverInfo":{"name":"fake","version":"0.0.1"}}})
    elif method == "notifications/initialized":
        continue
    elif method == "tools/call":
        sys.stdout.write("this is not json and never will be\n")
        sys.stdout.flush()
        emit({"jsonrpc":"2.0","id":mid,"result":{
            "content":[{"type":"text","text":"echoed"}],
            "isError":False}})
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn garbage_line_before_a_valid_response_does_not_break_correlation() {
    if !python_available() {
        eprintln!("skipping: python3 not on PATH");
        return;
    }
    let (_dir, client) = spawn_with_script(GARBAGE_BEFORE_VALID).await;
    client.initialize().await.expect("initialize");

    let result = client
        .call_tool("echo", serde_json::json!({}))
        .await
        .expect("a garbage line must be skipped, not surface as an error");
    let rendered = result.render_text();
    assert!(
        rendered.contains("echoed"),
        "the response after a garbage line must still correlate to its \
         pending request; got {rendered:?}",
    );
}

/// Delays only the FIRST `tools/call` reply so the client's short
/// timeout fires. Later calls are answered immediately. This lets one
/// test prove both that the timeout fires on schedule and that the
/// client is not poisoned.
const TIMEOUT_THEN_RESPOND: &str = r#"import json, sys, time
calls = 0
def emit(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    method = msg.get("method")
    mid = msg.get("id")
    if method == "initialize":
        emit({"jsonrpc":"2.0","id":mid,"result":{
            "protocolVersion":"2025-06-18",
            "capabilities":{"tools":{}},
            "serverInfo":{"name":"fake","version":"0.0.1"}}})
    elif method == "notifications/initialized":
        continue
    elif method == "tools/call":
        calls += 1
        if calls == 1:
            time.sleep(2)
        emit({"jsonrpc":"2.0","id":mid,"result":{
            "content":[{"type":"text","text":"call-"+str(calls)}],
            "isError":False}})
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timeout_on_one_call_does_not_poison_the_client() {
    if !python_available() {
        eprintln!("skipping: python3 not on PATH");
        return;
    }
    let (_dir, client) = spawn_with_script(TIMEOUT_THEN_RESPOND).await;
    client.initialize().await.expect("initialize");

    let t0 = std::time::Instant::now();
    let first = client
        .call_tool_with_timeout("echo", serde_json::json!({}), 1)
        .await;
    let first_elapsed = t0.elapsed();
    assert!(
        matches!(first, Err(kod_mcp::McpError::Timeout)),
        "the first call must time out (the server delays its reply); got {first:?}",
    );
    assert!(
        first_elapsed < Duration::from_secs(3),
        "the timeout must fire at ~1s, not wait for the server's 2s delay; \
         took {first_elapsed:?}",
    );

    // Second call: same client. The server's next reply is immediate.
    // If `request()` had left a stale pending entry, or if the timeout
    // had poisoned the client, this would fail.
    let second = client
        .call_tool_with_timeout("echo", serde_json::json!({}), 5)
        .await
        .expect("a timed-out request must not poison the client");
    let rendered = second.render_text();
    assert!(
        rendered.contains("call-2"),
        "the second call must reach the server and get its reply; got {rendered:?}",
    );
}
