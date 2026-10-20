//! Engine <-> MCP tool dispatch, end to end (session 16,
//! behavioral-integration arc).
//!
//! The `kod-mcp` crate has its own protocol tests (real Python fake
//! server, `initialize` / `tools/list` / `tools/call`). The
//! `mcp_adapters` module has tests for name composition and the
//! `ensure_started` error paths. What neither covers is the seam:
//! a config entry -> `install_from_config` -> `start()` registering
//! the adapter -> the engine's tool loop dispatching a model-supplied
//! `mcp:<server>.<tool>` call -> the adapter forwarding through a
//! real spawned server -> the rendered result reaching
//! `TaskResponse.tool_results`.
//!
//! This test walks that entire path with a `MockProvider` scripting
//! one tool call, then one text reply. The MCP server is a real
//! subprocess (a small Python script) — the same shape
//! `kod-mcp/src/client.rs::tests` uses, but driven through the engine
//! instead of the client's public methods directly. No model, no
//! network.

use kod_config::{McpConfig, McpServerConfig};
use kod_core::KodEngine;
use kod_core::router::RouterConfig;
use kod_provider::testkit::{MockProvider, Script};
use kod_types::ToolCall;
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::Arc;
use tempfile::TempDir;

/// Minimal MCP server. Same protocol surface as
/// `kod-mcp/src/client.rs::tests::FAKE_SERVER` — `initialize`,
/// `tools/list` (one `echo` tool), `tools/call` (echoes the `text`
/// argument). The only job of this test is to prove the engine can
/// reach it; the client's own tests already pin the wire details.
const FAKE_MCP_SERVER: &str = r#"import json, sys
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    method = msg.get("method")
    msg_id = msg.get("id")
    if method == "initialize":
        out = {"jsonrpc":"2.0","id":msg_id,"result":{
            "protocolVersion":"2025-06-18",
            "capabilities":{"tools":{}},
            "serverInfo":{"name":"fake","version":"0.0.1"}
        }}
    elif method == "notifications/initialized":
        continue
    elif method == "tools/list":
        out = {"jsonrpc":"2.0","id":msg_id,"result":{"tools":[
            {"name":"echo","description":"echo back","inputSchema":{
                "type":"object","properties":{"text":{"type":"string"}}
            }}
        ]}}
    elif method == "tools/call":
        name = msg["params"]["name"]
        if name == "echo":
            text = msg["params"]["arguments"].get("text","")
            out = {"jsonrpc":"2.0","id":msg_id,"result":{
                "content":[{"type":"text","text":"echo: "+text}],
                "isError":False
            }}
        else:
            out = {"jsonrpc":"2.0","id":msg_id,"error":{"code":-32601,"message":"no such tool"}}
    else:
        out = {"jsonrpc":"2.0","id":msg_id,"error":{"code":-32601,"message":"no such method"}}
    sys.stdout.write(json.dumps(out)+"\n")
    sys.stdout.flush()
"#;

fn python_available() -> bool {
    std::process::Command::new("python3")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Install a fake MCP server under `$tmp/fake-mcp/server.py` and
/// return the `McpConfig` that spawns it under the name `fake`.
///
/// The server name matters: `install_from_config` derives the adapter
/// name as `mcp:<server>.<tool>`, so the model's call must be
/// `mcp:fake.echo`.
fn mcp_config_with_fake_server(tmp: &std::path::Path) -> McpConfig {
    let dir = tmp.join("fake-mcp");
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("server.py");
    std::fs::write(&script, FAKE_MCP_SERVER).unwrap();

    let mut servers: BTreeMap<String, McpServerConfig> = BTreeMap::new();
    servers.insert(
        "fake".to_string(),
        McpServerConfig {
            command: "python3".to_string(),
            args: vec![script.to_string_lossy().to_string()],
            env: BTreeMap::new(),
            enabled: true,
            call_timeout_secs: 10,
        },
    );
    McpConfig { servers }
}

/// Build a started engine with a scripted provider and a fake MCP
/// server installed **before** `start()` — the point at which the
/// lifecycle registers the adapters on the tool registry.
async fn engine_with_mcp(tmp: &TempDir, script: Vec<Script>) -> Arc<KodEngine> {
    let cfg = RouterConfig {
        embedder: None,
        skill_threshold: 0.3,
        working_dir: tmp.path().to_path_buf(),
        enable_memory: false,
        max_skills_per_query: 3,
        context_window: 8192,
        short_term_capacity: 100,
    };
    let engine = Arc::new(KodEngine::new(cfg, tmp.path().join("mcp.redb")).unwrap());

    // Install the MCP host BEFORE start(). start()'s lifecycle reads
    // self.mcp and calls startup_tools(); set_mcp_host after start()
    // would register zero adapters (see the doc on set_mcp_host).
    let host = Arc::new(kod_core::mcp_adapters::McpHost::new(
        mcp_config_with_fake_server(tmp.path()),
        tmp.path().to_path_buf(),
    ));
    engine.set_mcp_host(host).await;

    // Scripted provider: one tool call, then a text reply. The
    // `scripted` constructor consumes entries in order, so the second
    // round sees Text, not the same tool call.
    let provider = Arc::new(MockProvider::scripted("mock", script));
    let mut registry = kod_provider::ProviderRegistry::new();
    registry.insert(
        "mock",
        provider,
        kod_provider::ProviderCapabilities::conservative(),
        "mock-model",
    );
    let mut routing = kod_config::RoutingConfig::default();
    routing
        .by_task
        .insert("Simple".to_string(), "mock".to_string());
    engine
        .set_registry(
            Arc::new(registry),
            kod_provider::ModelRef::new("mock", "mock-model"),
            Some(routing),
        )
        .await;

    engine.start().await.unwrap();
    engine
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn engine_dispatches_a_model_tool_call_through_a_real_mcp_server() {
    if !python_available() {
        eprintln!("skipping: python3 not on PATH");
        return;
    }

    // Round 1: the model asks for the MCP tool by its namespaced
    // name. Round 2: the model writes a closing message after seeing
    // the tool result.
    let call = ToolCall {
        id: Some("call-mcp-1".into()),
        tool_name: "mcp:fake.echo".into(),
        arguments: json!({ "text": "seam" }),
    };
    let script = vec![Script::Calls(vec![call]), Script::Text("done".into())];

    let tmp = TempDir::new().unwrap();
    let engine = engine_with_mcp(&tmp, script).await;

    // The turn itself proves the adapter registered: an unregistered
    // tool name would fail dispatch with "unknown tool" and the
    // rendered result below would not carry the server's echo text.
    let resp = engine.process("please echo").await.expect("process");

    // The turn dispatched the tool, got the server's echoed text
    // back, and surfaced it in the response's tool_results.
    assert_eq!(
        resp.tool_calls.len(),
        1,
        "one model tool call expected; got {:?}",
        resp.tool_calls,
    );
    assert_eq!(resp.tool_calls[0].tool_name, "mcp:fake.echo");
    assert_eq!(
        resp.tool_results.len(),
        1,
        "one tool result expected; got {:?}",
        resp.tool_results,
    );
    // The adapter wraps a success into a JSON blob carrying server,
    // tool, text. Match on any of those to stay robust to future
    // shape tweaks; the important assertion is that the server's
    // response text reached the engine.
    let rendered = format!("{:?}", resp.tool_results[0]);
    assert!(
        rendered.contains("echo: seam"),
        "the real MCP server's reply must reach tool_results; got {rendered}",
    );

    engine.shutdown().await.ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn install_from_config_registers_the_adapter_on_a_started_engine() {
    if !python_available() {
        eprintln!("skipping: python3 not on PATH");
        return;
    }

    let tmp = TempDir::new().unwrap();
    let cfg = RouterConfig {
        embedder: None,
        skill_threshold: 0.3,
        working_dir: tmp.path().to_path_buf(),
        enable_memory: false,
        max_skills_per_query: 3,
        context_window: 8192,
        short_term_capacity: 100,
    };
    let engine = Arc::new(KodEngine::new(cfg, tmp.path().join("mcp2.redb")).unwrap());

    // The production installer: reads [mcp.servers] from KodConfig,
    // builds the host, sets it on the engine. It does not itself
    // register tools — start() does, which is why the set_mcp_host
    // doc says the tool loop only sees adapters registered at start().
    let kod_config = kod_config::KodConfig {
        mcp: mcp_config_with_fake_server(tmp.path()),
        ..Default::default()
    };
    kod_core::mcp_adapters::install_from_config(&engine, &kod_config).await;

    // Provide a provider so start() succeeds (it requires one for the
    // chat path, even though this test never runs a turn).
    let provider = Arc::new(MockProvider::new("mock", Script::Text("unused".into())));
    let mut registry = kod_provider::ProviderRegistry::new();
    registry.insert(
        "mock",
        provider,
        kod_provider::ProviderCapabilities::conservative(),
        "mock-model",
    );
    engine
        .set_registry(
            Arc::new(registry),
            kod_provider::ModelRef::new("mock", "mock-model"),
            None,
        )
        .await;
    engine.start().await.unwrap();

    // Prove install_from_config propagated the [mcp.servers] block to
    // the engine and start() registered the adapter: a turn that
    // names the tool must dispatch to the real fake server and
    // surface its response in tool_results. This is the seam that
    // neither kod-mcp's protocol tests nor mcp_adapters' unit tests
    // can cover — those test the parts, this test tests the join.
    //
    // The engine's tool registry is private; the dispatch result IS
    // the registration check. Swap the script to a tool call.
    let call = ToolCall {
        id: Some("call-mcp-2".into()),
        tool_name: "mcp:fake.echo".into(),
        arguments: json!({ "text": "from-config" }),
    };
    let provider = Arc::new(MockProvider::scripted(
        "mock",
        vec![Script::Calls(vec![call]), Script::Text("done".into())],
    ));
    let mut registry = kod_provider::ProviderRegistry::new();
    registry.insert(
        "mock",
        provider,
        kod_provider::ProviderCapabilities::conservative(),
        "mock-model",
    );
    engine
        .set_registry(
            Arc::new(registry),
            kod_provider::ModelRef::new("mock", "mock-model"),
            None,
        )
        .await;

    let resp = engine
        .process("please echo from config")
        .await
        .expect("process");
    assert_eq!(
        resp.tool_results.len(),
        1,
        "one tool result expected after install_from_config; got {:?}",
        resp.tool_results,
    );
    let rendered = format!("{:?}", resp.tool_results[0]);
    assert!(
        rendered.contains("echo: from-config"),
        "install_from_config's adapter must reach the real server; got {rendered}",
    );

    engine.shutdown().await.ok();
}
