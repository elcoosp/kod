//! Policy gate integration tests (D3-C1b + C2a).
//!
//! The engine consults its `PolicyEngine` (when installed) before
//! every tool call. These tests exercise the end-to-end contract:
//!
//! - `ReadOnly` denies `write_file` with a Deny tool result carrying
//!   the policy rule, and the file is not created.
//! - `Yolo` allows everything; the write lands.
//! - A per-tool `[tools.write_file] mode = "deny"` overrides `Yolo`.
//! - A session deny rule wins over an `Allow` policy.
//! - Without a policy, the legacy `confirm_writes` default lets the
//!   write through.
//! - DenyAlways registers a session rule (the TUI 'a' key's server
//!   side), and a matching rule blocks a write without an approval.

use async_trait::async_trait;
use futures::Stream;
use kod_config::{Decision, Policy, PolicyEngine, Preset, ToolPolicy};
use kod_core::{KodEngine, RouterConfig};
use kod_error::Result;
use kod_provider::{GenerationOptions, GenerationResponse, LlmProvider, StreamChunk};
use kod_types::{ToolCall, ToolDefinition, ToolResult};
use std::collections::BTreeMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

#[path = "common/install_test_provider.rs"]
mod install_test_provider_mod;
use install_test_provider_mod::install_test_provider;

/// A provider that emits a fixed tool-call sequence on the first call,
/// then a plain-text reply. `calls` is consumed in order: one entry
/// per tool round; an empty entry means "no more tool calls, answer
/// in text".
struct ScriptedProvider {
    calls: Mutex<Vec<Vec<ToolCall>>>,
}

impl ScriptedProvider {
    fn new(calls: Vec<Vec<ToolCall>>) -> Self {
        Self {
            calls: Mutex::new(calls),
        }
    }
}

#[async_trait]
impl LlmProvider for ScriptedProvider {
    fn name(&self) -> &str {
        "scripted"
    }
    async fn list_models(&self) -> Result<Vec<kod_provider::ModelInfo>> {
        Ok(vec![])
    }
    async fn generate(&self, _p: &str, _o: &GenerationOptions) -> Result<String> {
        Ok("done".into())
    }
    async fn generate_with_tools(
        &self,
        _p: &str,
        _t: &[ToolDefinition],
        _o: &GenerationOptions,
    ) -> Result<GenerationResponse> {
        let mut g = self.calls.lock().unwrap();
        let calls = if g.is_empty() {
            Vec::new()
        } else {
            g.remove(0)
        };
        if calls.is_empty() {
            Ok(GenerationResponse::Text {
                content: "done".into(),
                usage: None,
            })
        } else {
            Ok(GenerationResponse::ToolCalls { calls, usage: None })
        }
    }
    fn stream(
        &self,
        _p: &str,
        _o: &GenerationOptions,
    ) -> Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send + '_>> {
        Box::pin(futures::stream::empty())
    }
}

fn engine_in(dir: &std::path::Path) -> KodEngine {
    std::fs::write(dir.join("main.rs"), "pub fn main() {}\n").unwrap();
    let db_path = dir.join("test.redb");
    let cfg = RouterConfig {
        embedder: None,
        skill_threshold: 0.3,
        working_dir: dir.to_path_buf(),
        enable_memory: false,
        max_skills_per_query: 3,
        context_window: 8192,
        short_term_capacity: 100,
    };
    KodEngine::new(cfg, db_path).unwrap()
}

fn write_call(path: &str) -> Vec<ToolCall> {
    vec![ToolCall {
        id: None,
        tool_name: "write_file".to_string(),
        arguments: serde_json::json!({
            "path": path,
            "content": "fn main() {}\n",
        }),
    }]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_only_preset_denies_write_file() {
    let temp = TempDir::new().unwrap();
    let engine = engine_in(temp.path());
    let provider = Arc::new(ScriptedProvider::new(vec![write_call("out.txt")]));
    install_test_provider(&engine, provider).await;

    let effective = Policy {
        preset: Preset::ReadOnly,
        ..Policy::default()
    };
    let pe = PolicyEngine::from_effective_for_tests(effective);
    engine.set_policy(Arc::new(pe)).await;

    engine.start().await.unwrap();
    let resp = engine.process("write out.txt").await.unwrap();

    assert_eq!(resp.tool_results.len(), 1);
    match &resp.tool_results[0] {
        ToolResult::Error(msg) => {
            assert!(
                msg.contains("policy") || msg.contains("deny") || msg.contains("preset"),
                "unexpected error: {msg}"
            );
        }
        other => panic!("expected a policy denial, got {other:?}"),
    }
    assert!(!temp.path().join("out.txt").exists());

    engine.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn yolo_preset_allows_write_file() {
    let temp = TempDir::new().unwrap();
    let engine = engine_in(temp.path());
    let provider = Arc::new(ScriptedProvider::new(vec![write_call("out.txt")]));
    install_test_provider(&engine, provider).await;

    let effective = Policy {
        preset: Preset::Yolo,
        ..Policy::default()
    };
    let pe = PolicyEngine::from_effective_for_tests(effective);
    engine.set_policy(Arc::new(pe)).await;

    engine.start().await.unwrap();
    let resp = engine.process("write out.txt").await.unwrap();

    assert_eq!(resp.tool_results.len(), 1);
    match &resp.tool_results[0] {
        ToolResult::Success(_) => {}
        other => panic!("expected success, got {other:?}"),
    }
    assert!(temp.path().join("out.txt").exists());

    engine.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn per_tool_deny_overrides_yolo() {
    let temp = TempDir::new().unwrap();
    let engine = engine_in(temp.path());
    let provider = Arc::new(ScriptedProvider::new(vec![write_call("out.txt")]));
    install_test_provider(&engine, provider).await;

    let mut tools = BTreeMap::new();
    tools.insert(
        "write_file".to_string(),
        ToolPolicy {
            mode: Some(Decision::Deny),
            ..ToolPolicy::default()
        },
    );
    let effective = Policy {
        preset: Preset::Yolo,
        tools,
        ..Policy::default()
    };
    let pe = PolicyEngine::from_effective_for_tests(effective);
    engine.set_policy(Arc::new(pe)).await;

    engine.start().await.unwrap();
    let resp = engine.process("write out.txt").await.unwrap();

    match &resp.tool_results[0] {
        ToolResult::Error(_) => {}
        other => panic!("expected denial, got {other:?}"),
    }
    assert!(!temp.path().join("out.txt").exists());

    engine.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_deny_rule_wins_over_allow() {
    let temp = TempDir::new().unwrap();
    let engine = engine_in(temp.path());
    let provider = Arc::new(ScriptedProvider::new(vec![write_call("out.txt")]));
    install_test_provider(&engine, provider).await;

    let effective = Policy {
        preset: Preset::Yolo,
        ..Policy::default()
    };
    let pe = PolicyEngine::from_effective_for_tests(effective);
    engine.set_policy(Arc::new(pe)).await;
    engine
        .add_deny_rule(kod_config::SessionDeny {
            tool: "write_file".to_string(),
            path_pattern: Some("out.txt".to_string()),
        })
        .await;

    engine.start().await.unwrap();
    let resp = engine.process("write out.txt").await.unwrap();

    match &resp.tool_results[0] {
        ToolResult::Error(msg) => {
            assert!(
                msg.contains("session deny"),
                "expected session-deny message, got: {msg}"
            );
        }
        other => panic!("expected denial, got {other:?}"),
    }
    assert!(!temp.path().join("out.txt").exists());

    engine.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_policy_allows_every_call() {
    // Without set_policy, the engine allows every call — the pre-policy
    // default. The CLI and TUI always install a policy at startup; the
    // fallback exists for tests and embedders.
    let temp = TempDir::new().unwrap();
    let engine = engine_in(temp.path());
    let provider = Arc::new(ScriptedProvider::new(vec![write_call("out.txt")]));
    install_test_provider(&engine, provider).await;
    engine.start().await.unwrap();

    let resp = engine.process("write out.txt").await.unwrap();
    match &resp.tool_results[0] {
        ToolResult::Success(_) => {}
        other => panic!("expected success with no policy installed, got {other:?}"),
    }
    assert!(temp.path().join("out.txt").exists());

    engine.shutdown().await.unwrap();
}

/// DenyAlways registers a session deny rule on the engine, so a
/// second matching call is denied without any policy prompt. This is
/// the TUI 'a' key's server-side contract.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deny_always_registers_session_rule() {
    let temp = TempDir::new().unwrap();
    let engine = engine_in(temp.path());
    engine.start().await.unwrap();

    assert!(engine.deny_rules().await.is_empty());

    let rule = kod_config::SessionDeny {
        tool: "write_file".to_string(),
        path_pattern: Some("src/**".to_string()),
    };
    engine.add_deny_rule(rule.clone()).await;

    let rules = engine.deny_rules().await;
    assert_eq!(rules.len(), 1);
    assert!(rules.contains(&rule));

    // Adding the same rule twice is idempotent (HashSet semantics).
    engine.add_deny_rule(rule.clone()).await;
    assert_eq!(engine.deny_rules().await.len(), 1);

    engine.shutdown().await.unwrap();
}
