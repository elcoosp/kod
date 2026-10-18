//! Real-MCP-server integration (session 18, operational-integration arc).
//!
//! The `client.rs` unit tests drive a *hand-written Python fake* MCP
//! server. That fake is the right thing for the protocol tests — it
//! is deterministic, no network, runs everywhere `python3` is on
//! PATH. But it exercises a server whose JSON the client author also
//! wrote; protocol drift on a real implementation is invisible.
//!
//! This file drives `McpClient` against the reference
//! `@modelcontextprotocol/server-filesystem`, spawned via `npx`. It is
//! `#[ignore]`d by default — the same convention
//! `kod-provider-anthropic/tests/it/live.rs` and
//! `kod-tools/tests/it/sandbox_containment.rs` already use — because
//! it needs Node and `npx` on `$PATH`, which the default test run
//! does not guarantee.
//!
//! To run manually:
//!
//!     cargo nextest run -p kod-mcp --run-ignored -E 'test(real_filesystem)'
//!
//! First run downloads `@modelcontextprotocol/server-filesystem` via
//! `npx -y` (skips npm's install prompt, but needs network the first
//! time). To skip cleanly even when `--run-ignored` is passed, the
//! tests check `npx_available()` and return early — they do not fail.
//!
//! What this catches that the Python fake does not:
//!   - server name and version negotiation against a real `initialize`;
//!   - the real tool set a real filesystem server advertises (a
//!     rename or removal fails loudly, where the fake would silently
//!     stay green);
//!   - `tools/call` against a real filesystem tool with real
//!     arguments and a real response;
//!   - real shutdown semantics (Node's SIGTERM handler vs the fake's
//!     Python loop).

use kod_mcp::McpClient;
use std::collections::BTreeMap;
use tempfile::TempDir;

/// True if `npx` is on `$PATH`. Same shape as the (private)
/// `kod-lsp/src/client.rs::binary_on_path` helper and the
/// `python_available` guard in `kod-mcp`'s own unit tests. Two lines;
/// cheap to duplicate rather than lift into `kod-mcp`'s public API.
fn npx_available() -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join("npx").is_file()))
        .unwrap_or(false)
}

/// Spawn the reference filesystem MCP server rooted at `dir`.
///
/// `-y` skips npm's "install this package?" prompt; without it a
/// first-time run blocks on stdin waiting for a TTY answer. Version
/// is `@latest` on purpose: this is a drift-detection test — pinning
/// a version would pass forever while the real server moved on.
async fn spawn_filesystem_server(dir: &std::path::Path) -> McpClient {
    let args = vec![
        "-y".to_string(),
        "@modelcontextprotocol/server-filesystem".to_string(),
        dir.to_string_lossy().to_string(),
    ];
    McpClient::spawn_stdio("npx", &args, &BTreeMap::new())
        .await
        .expect("spawn @modelcontextprotocol/server-filesystem via npx")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "operational: needs npx + Node on PATH; run with --run-ignored"]
async fn real_filesystem_server_initialize_and_list_tools() {
    if !npx_available() {
        eprintln!("skipping: npx not on PATH");
        return;
    }

    let dir = TempDir::new().expect("tempdir");
    let client = spawn_filesystem_server(dir.path()).await;

    // initialize: the server names itself. The exact name varies by
    // version; assert only non-empty — a server that fails to name
    // itself is a protocol violation, not a version bump.
    let info = client.initialize().await.expect("initialize");
    assert!(
        !info.name.is_empty(),
        "real filesystem server must name itself; got {info:?}",
    );
    assert!(
        !info.version.is_empty(),
        "real filesystem server must report a version; got {info:?}",
    );

    // tools/list: assert the core filesystem tools are present. The
    // exact set varies by version; a real server will always expose
    // read_file and list_directory. Asserting those two by name is the
    // drift signal — if the server renames or removes them, this test
    // fails loudly while the Python fake stays green.
    let tools = client.list_tools().await.expect("tools/list");
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    assert!(
        names.contains(&"read_file"),
        "reference filesystem server must expose read_file; got {names:?}",
    );
    assert!(
        names.contains(&"list_directory"),
        "reference filesystem server must expose list_directory; got {names:?}",
    );

    // Every tool must carry an inputSchema. A server that returns
    // tools without one is a client-visible protocol violation.
    for t in &tools {
        assert!(
            !t.input_schema.is_null(),
            "tool {} must carry inputSchema; got {:?}",
            t.name,
            t.input_schema,
        );
    }

    client.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "operational: needs npx + Node on PATH; run with --run-ignored"]
async fn real_filesystem_server_reads_a_written_file() {
    if !npx_available() {
        eprintln!("skipping: npx not on PATH");
        return;
    }

    // Write a file the server can read. The server is rooted at
    // `dir.path()`; absolute paths inside that root are allowed.
    let dir = TempDir::new().expect("tempdir");
    let target = dir.path().join("hello.txt");
    std::fs::write(&target, "hello from a real MCP filesystem server\n").expect("write target");

    let client = spawn_filesystem_server(dir.path()).await;
    client.initialize().await.expect("initialize");

    // read_file: real path, real content.
    let args = serde_json::json!({ "path": target.to_string_lossy() });
    let result = client.call_tool("read_file", args).await.expect("read_file");
    let rendered = result.render_text();
    assert!(
        rendered.contains("hello from a real MCP filesystem server"),
        "read_file must return the file content; got {rendered:?}",
    );
    assert!(
        !result.is_error,
        "read_file of an existing file must not report an error; got {result:?}",
    );

    // list_directory: real path, real listing.
    let args = serde_json::json!({ "path": dir.path().to_string_lossy() });
    let listing = client
        .call_tool("list_directory", args)
        .await
        .expect("list_directory");
    let rendered = listing.render_text();
    assert!(
        rendered.contains("hello.txt"),
        "list_directory must name the file it was just told about; got {rendered:?}",
    );

    client.shutdown().await;
}
