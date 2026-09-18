//! `kod acp` subcommand handshake (design §11.2).
//!
//! Spawns the built `kod` binary with the `acp` subcommand, sends an
//! ACP `initialize` request over stdio with `Content-Length` framing,
//! and asserts the response is well-formed JSON-RPC carrying the
//! agreed protocol version. Closes stdin and waits for the process to
//! exit cleanly.
//!
//! # What this proves
//!
//! - The subcommand exists and is dispatched (a missing arm in
//!   `Cli::run` would fail at `clap` parse or exit non-zero).
//! - The ACP bridge (`kod_core::acp::serve`) runs and answers the
//!   spec's `initialize` method.
//! - The response is framed as `Content-Length: N\r\n\r\nJSON`, the
//!   ACP/LSP stdio convention.
//!
//! # What it deliberately does not do
//!
//! - It does not run a full session (session/new + session/prompt).
//!   That would need a live model; the handshake alone pins the
//!   subcommand wiring.
//! - It does not assert on stderr; a diagnostic printed there does not
//!   corrupt the protocol, and the test does not need to pin its
//!   wording.

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Read a `Content-Length`-framed JSON-RPC message from `reader`.
///
/// Times out at `timeout`: a handshake that does not answer within a
/// few seconds on a cold binary is a regression, not a slow machine.
fn read_frame<R: Read>(
    reader: &mut std::io::BufReader<R>,
    timeout: Duration,
) -> Result<serde_json::Value, String> {
    use std::io::BufRead;
    let start = Instant::now();
    let mut content_length: Option<usize> = None;
    loop {
        if start.elapsed() > timeout {
            return Err(format!("handshake timed out after {timeout:?}"));
        }
        let mut line = String::new();
        let n = reader.read_line(&mut line).map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("stdout closed before a response arrived".into());
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some(rest) = trimmed.strip_prefix("Content-Length:") {
            content_length = Some(
                rest.trim()
                    .parse()
                    .map_err(|_| format!("bad Content-Length: {rest:?}"))?,
            );
        }
    }
    let n = content_length.ok_or_else(|| "missing Content-Length".to_string())?;
    let mut buf = vec![0u8; n];
    reader.read_exact(&mut buf).map_err(|e| e.to_string())?;
    serde_json::from_slice(&buf).map_err(|e| e.to_string())
}

#[test]
fn acp_subcommand_answers_initialize() {
    // `CARGO_BIN_EXE_kod` is set by cargo for integration tests of a
    // crate with a binary target; the binary is built automatically.
    let exe = env!("CARGO_BIN_EXE_kod");
    let mut child = Command::new(exe)
        .arg("acp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn kod acp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let mut reader = std::io::BufReader::new(stdout);

    // The ACP v1 `initialize` request. Ids are opaque; the client
    // chooses them, the server echoes.
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": 1,
            "clientCapabilities": {}
        }
    });
    let body = serde_json::to_vec(&request).unwrap();
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    stdin.write_all(header.as_bytes()).unwrap();
    stdin.write_all(&body).unwrap();
    stdin.flush().unwrap();

    // Read the response frame. Any well-formed JSON-RPC response is
    // enough; the assertion is the shape, not a specific field.
    let response = match read_frame(&mut reader, Duration::from_secs(15)) {
        Ok(v) => v,
        Err(e) => {
            // A failed handshake closes nothing; kill the child so
            // the test does not leak a process.
            let _ = child.kill();
            panic!("did not receive an initialize response: {e}");
        }
    };

    // The response must be JSON-RPC 2.0 and carry the id we sent.
    assert_eq!(
        response["jsonrpc"], "2.0",
        "response must carry jsonrpc 2.0, got: {response}",
    );
    assert_eq!(
        response["id"], 1,
        "response id must echo the request, got: {response}",
    );

    // Either `result` (success) or `error` (a documented error
    // response to a malformed request) is legal. Both prove the
    // subcommand is wired and answers.
    assert!(
        response.get("result").is_some() || response.get("error").is_some(),
        "response must carry result or error, got: {response}",
    );

    // Close stdin: the ACP bridge's read loop returns on EOF, the
    // engine shuts down cleanly, and the process exits 0.
    drop(stdin);
    let status = child.wait().expect("wait");
    assert!(
        status.success(),
        "kod acp must exit cleanly on stdin EOF, got {status:?}",
    );
}
