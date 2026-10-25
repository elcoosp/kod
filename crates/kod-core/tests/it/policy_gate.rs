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

use crate::common::install_test_provider::install_test_provider;

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

/// Regression: an approval wait must observe the transcript's cancel
/// flag. Pre-fix, `run_tool_calls` awaited the oneshot with a bare
/// `tokio::time::timeout(AWAIT_APPROVAL_SECS, rx)` — Esc / `kod serve
/// cancel` while an approval was pending did nothing until the full
/// 120 s window elapsed, then the loop's check on the way out finally
/// noticed the cancel. The user saw a frozen dialog for two minutes
/// after already having asked to stop.
///
/// This test starts a `process_streaming` call whose write_file is
/// held at the approval gate, waits for the approval marker to reach
/// the chunk channel, calls `request_cancel`, and then asserts the
/// call returns well inside `AWAIT_APPROVAL_SECS`. A pre-fix build
/// hangs here until the 120 s timeout fires.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_while_pending_approval_does_not_wait_the_timeout() {
    use kod_core::engine::parse_tool_approval_batch;
    use tokio::sync::mpsc;

    let temp = TempDir::new().unwrap();
    let engine = Arc::new(engine_in(temp.path()));
    let provider = Arc::new(ScriptedProvider::new(vec![write_call("out.txt")]));
    install_test_provider(&engine, provider).await;

    let effective = Policy {
        preset: Preset::Standard,
        ..Policy::default()
    };
    let pe = PolicyEngine::from_effective_for_tests(effective);
    engine.set_policy(Arc::new(pe)).await;
    engine.start().await.unwrap();

    let (tx, mut rx) = mpsc::channel::<String>(64);
    let engine_for_task = Arc::clone(&engine);
    let run = tokio::spawn(async move {
        engine_for_task
            .process_streaming("write out.txt", &tx)
            .await
    });

    // Wait for the approval marker. The engine emits a single batch
    // marker before parking on the per-item oneshot; seeing it means
    // the await has started.
    let mut saw_approval = false;
    let wait_for_marker = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while let Some(chunk) = rx.recv().await {
            if parse_tool_approval_batch(&chunk).is_some() {
                saw_approval = true;
                break;
            }
        }
    })
    .await;
    assert!(
        wait_for_marker.is_ok() && saw_approval,
        "the engine must emit an approval batch marker for a Standard-preset write",
    );

    // Cancel. Pre-fix, the parked `timeout(rx)` ignores this.
    engine.request_cancel();

    // The call must return promptly. `AWAIT_APPROVAL_SECS` is 120 s;
    // we budget 10 s, which is generous for the 100 ms poll interval
    // plus scheduler noise.
    let finished = tokio::time::timeout(std::time::Duration::from_secs(10), run).await;
    assert!(
        finished.is_ok(),
        "process_streaming did not return within 10 s of request_cancel — \
         the approval wait ignored the cancel flag",
    );
    // `run` is a `JoinHandle<Result<TaskResponse>>`, so unwrap the
    // outer timeout and the join. The result itself is *expected to
    // be an error* — the approval wait observes the cancel and
    // returns `DecisionWait::Cancelled`, which the approval flow
    // turns into a "cancelled" decision; the round loop then sees
    // the still-set cancel flag on its next iteration and returns
    // `InvalidState("cancelled by user")`. That is the same shape
    // every other cancel path produces (see
    // `tool_loop::test_cancel_stops_process`), so callers that
    // already handle the cancel error keep working.
    let joined = finished.expect("process_streaming returned inside the cancel budget");
    let stream_result = joined.expect("spawned process_streaming task must not panic");
    let err = match stream_result {
        Ok(resp) => {
            // A `Standard`-preset write_file should not have been
            // allowed through: if the call somehow returned Ok, the
            // cancel did not take effect and the approval must have
            // been answered.
            panic!(
                "a cancelled approval must not resolve Ok; got {:?}",
                resp.tool_calls
            );
        }
        Err(e) => e,
    };
    assert!(
        matches!(err, kod_error::KodError::InvalidState(_)),
        "expected InvalidState, got {err:?}",
    );
    assert!(
        err.to_string().contains("cancel"),
        "error must name the cancel: {err}",
    );

    // The critical assertion: the write was never allowed through.
    // A pre-fix build would have waited out the full 120s
    // `AWAIT_APPROVAL_SECS` — the outer 10s timeout above already
    // fails on that, but this pins the side-effect-free outcome
    // independently.
    assert!(
        !temp.path().join("out.txt").exists(),
        "a cancelled approval must not have allowed the write",
    );

    engine.shutdown().await.unwrap();
}

/// The `list_files` counterpart of the grep / search_files / jfind
/// end-to-end tests. `list_files` shares the `34961905` fix with
/// `grep` (both in `kod-tools`), but only `grep`'s through-engine
/// path was proven. This closes the symmetry: every walk tool in
/// the workspace has an end-to-end test proving the engine-installed
/// `forbidden_paths` reaches its per-file filter.
///
/// Fixture: `secrets/secret.txt` (forbidden) and `readme.txt`
/// (allowed). The directory branch of `list_files` walks recursively;
/// the `files` array must contain `readme.txt` and must NOT contain
/// `secrets/secret.txt`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_files_skips_entries_under_forbidden_paths_through_the_engine() {
    let temp = TempDir::new().unwrap();
    std::fs::create_dir_all(temp.path().join("secrets")).unwrap();
    std::fs::write(temp.path().join("secrets/secret.txt"), "x\n").unwrap();
    std::fs::write(temp.path().join("readme.txt"), "x\n").unwrap();

    let engine = engine_in(temp.path());

    let call = ToolCall {
        id: None,
        tool_name: "list_files".to_string(),
        arguments: serde_json::json!({
            "path": ".",
            "recursive": true,
        }),
    };
    let provider = Arc::new(ScriptedProvider::new(vec![vec![call]]));
    install_test_provider(&engine, provider).await;

    let effective = Policy {
        tools: std::collections::BTreeMap::from([(
            "list_files".to_string(),
            ToolPolicy {
                // `mode = Allow`: without it `gate_tool_calls` maps the
                // unset mode to Standard's Ask, and an Ask with no
                // interactive consumer is a hard deny at the tool
                // boundary -- which masks the walk-level check this
                // test is meant to exercise.
                mode: Some(Decision::Allow),
                forbidden: Some(vec!["**/secrets/**".to_string()]),
                ..Default::default()
            },
        )]),
        ..Policy::default()
    };
    let pe = PolicyEngine::from_effective_for_tests(effective);
    engine.set_policy(Arc::new(pe)).await;

    engine.start().await.unwrap();
    let resp = engine.process("list the files").await.unwrap();

    assert_eq!(
        resp.tool_results.len(),
        1,
        "the scripted provider must produce exactly one tool result; \
         got {} results",
        resp.tool_results.len(),
    );
    let v = match &resp.tool_results[0] {
        ToolResult::Success(v) => v,
        other => panic!("expected list_files success, got {other:?}"),
    };
    let files = v
        .get("files")
        .and_then(|x| x.as_array())
        .expect("list_files output has `files` array");
    let file_strs: Vec<&str> = files.iter().filter_map(|f| f.as_str()).collect();

    assert!(
        file_strs.iter().any(|s| s.contains("readme.txt")),
        "positive control failed: `readme.txt` must be listed; got {file_strs:?}",
    );
    // `forbidden_paths` uses per-path glob matching. `**/secrets/**`
    // matches paths *under* `secrets/` -- so `secrets/secret.txt` is
    // filtered, but the directory entry `secrets` itself is not a
    // match and may appear in the listing. A user who wants the
    // directory hidden too writes both `**/secrets` and
    // `**/secrets/**` in their `forbidden` glob list. This test pins
    // the documented semantic: no file *under* a forbidden path is
    // enumerated.
    assert!(
        !file_strs.iter().any(|s| s.contains("secrets/")),
        "walk must not enumerate files under a forbidden path; got {file_strs:?}",
    );

    // `total` is the pre-truncation count of the same filtered set
    // (5000-entry cap; this fixture is three entries, so no
    // truncation). Equality is exact.
    let total = v.get("total").and_then(|x| x.as_u64()).unwrap_or(0);
    assert_eq!(
        total,
        file_strs.len() as u64,
        "`total` must equal the filtered file count; got {total} for {file_strs:?}",
    );
}

/// The `jfind` counterpart of the grep / search_files end-to-end
/// tests. `JfindTool` lives in `kod-core` (not `kod-tools`), so this
/// is the only walk tool whose per-file check is in a different
/// crate -- proving the union wiring at c4c5f0df reaches it too.
///
/// Same fixture shape as `jfind_skips_files_under_forbidden_paths`:
/// a positive-control file (`secret-notes.txt`) and a forbidden file
/// (`secrets/secret.txt`), both matching the query. The control must
/// appear; the forbidden file must not.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn jfind_walks_skip_forbidden_paths_through_the_engine() {
    let temp = TempDir::new().unwrap();
    std::fs::create_dir_all(temp.path().join("secrets")).unwrap();
    std::fs::write(temp.path().join("secrets/secret.txt"), "secret data\n").unwrap();
    std::fs::write(temp.path().join("secret-notes.txt"), "secret data\n").unwrap();

    let engine = engine_in(temp.path());

    let call = ToolCall {
        id: None,
        tool_name: "jfind".to_string(),
        arguments: serde_json::json!({
            "query": "secret",
            "keywords": ["secret"],
        }),
    };
    let provider = Arc::new(ScriptedProvider::new(vec![vec![call]]));
    install_test_provider(&engine, provider).await;

    let effective = Policy {
        tools: std::collections::BTreeMap::from([(
            "jfind".to_string(),
            ToolPolicy {
                mode: Some(kod_config::Decision::Allow),
                forbidden: Some(vec!["**/secrets/**".to_string()]),
                ..Default::default()
            },
        )]),
        ..Policy::default()
    };
    let pe = PolicyEngine::from_effective_for_tests(effective);
    engine.set_policy(Arc::new(pe)).await;

    engine.start().await.unwrap();
    let resp = engine.process("find the secret").await.unwrap();

    assert_eq!(
        resp.tool_results.len(),
        1,
        "the scripted provider must produce exactly one tool result; \
         got {} results",
        resp.tool_results.len(),
    );
    let json_str = match &resp.tool_results[0] {
        ToolResult::Success(v) => v.to_string(),
        other => panic!("expected jfind success, got {other:?}"),
    };
    let v: serde_json::Value = serde_json::from_str(&json_str).expect("jfind output is JSON");
    let results = v
        .get("results")
        .and_then(|x| x.as_array())
        .expect("jfind output has `results` array");

    let has_notes = results.iter().any(|h| {
        h.get("file")
            .and_then(|x| x.as_str())
            .map(|s| s.contains("secret-notes.txt"))
            .unwrap_or(false)
    });
    assert!(
        has_notes,
        "positive control failed: `secret-notes.txt` must be reported; got {results:?}",
    );
    let has_forbidden = results.iter().any(|h| {
        h.get("file")
            .and_then(|x| x.as_str())
            .map(|s| s.contains("secrets/"))
            .unwrap_or(false)
    });
    assert!(
        !has_forbidden,
        "walk must not report files under a forbidden path; got {results:?}",
    );
}

/// The `search_files` counterpart of
/// `grep_walks_skip_forbidden_paths_through_the_engine`. Same shape,
/// different tool. Proves the walk-shaped tools (grep, list_files,
/// search_files) all honor the engine-installed `forbidden_paths`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_files_walks_skip_forbidden_paths_through_the_engine() {
    let temp = TempDir::new().unwrap();
    std::fs::create_dir_all(temp.path().join("secrets")).unwrap();
    std::fs::write(temp.path().join("secrets/creds.txt"), "SECRET_MARKER\n").unwrap();
    std::fs::write(temp.path().join("readme.txt"), "not a secret\n").unwrap();

    let engine = engine_in(temp.path());

    let call = ToolCall {
        id: None,
        tool_name: "search_files".to_string(),
        arguments: serde_json::json!({
            "path": ".",
            "pattern": "SECRET_MARKER",
        }),
    };
    let provider = Arc::new(ScriptedProvider::new(vec![vec![call]]));
    install_test_provider(&engine, provider).await;

    let effective = Policy {
        tools: std::collections::BTreeMap::from([(
            "search_files".to_string(),
            ToolPolicy {
                forbidden: Some(vec!["**/secrets/**".to_string()]),
                ..Default::default()
            },
        )]),
        ..Policy::default()
    };
    let pe = PolicyEngine::from_effective_for_tests(effective);
    engine.set_policy(Arc::new(pe)).await;

    engine.start().await.unwrap();
    let resp = engine.process("find the secret").await.unwrap();

    assert_eq!(
        resp.tool_results.len(),
        1,
        "the scripted provider must produce exactly one tool result; \
         got {} results",
        resp.tool_results.len(),
    );
    let json_str = match &resp.tool_results[0] {
        ToolResult::Success(v) => v.to_string(),
        other => panic!("expected search_files success, got {other:?}"),
    };
    let v: serde_json::Value =
        serde_json::from_str(&json_str).expect("search_files output is JSON");
    let results = v
        .get("results")
        .and_then(|x| x.as_array())
        .expect("search_files output has `results` array");
    for hit in results {
        let file = hit.get("file").and_then(|x| x.as_str()).unwrap_or("");
        assert!(
            !file.contains("secrets"),
            "walk must not read files under `forbidden`; got hit in {file:?}",
        );
    }
    assert!(results.is_empty(), "no match expected; got {results:?}",);
}

/// Bug-hunt addendum 20: the wiring fix at `c4c5f0df` installs the
/// per-round union of every call's `forbidden` globs onto
/// `ToolContext::permissions.forbidden_paths`. This test proves the
/// wiring reaches a walk tool: `grep` over `.` must skip files
/// under a `[policy.tools.grep] forbidden = [...]` glob.
///
/// Without the wiring fix, the walker saw an empty
/// `permissions.forbidden_paths` and read `secrets/creds.txt`. Without
/// the per-file check at `34961905`, the walker would have read it even
/// with the field populated. Both together: the walk skips the file.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grep_walks_skip_forbidden_paths_through_the_engine() {
    let temp = TempDir::new().unwrap();
    std::fs::create_dir_all(temp.path().join("secrets")).unwrap();
    std::fs::write(temp.path().join("secrets/creds.txt"), "SECRET_MARKER\n").unwrap();
    std::fs::write(temp.path().join("readme.txt"), "not a secret\n").unwrap();

    let engine = engine_in(temp.path());

    let call = ToolCall {
        id: None,
        tool_name: "grep".to_string(),
        arguments: serde_json::json!({
            "path": ".",
            "pattern": "SECRET_MARKER",
            "recursive": true,
        }),
    };
    let provider = Arc::new(ScriptedProvider::new(vec![vec![call]]));
    install_test_provider(&engine, provider).await;

    let effective = Policy {
        tools: std::collections::BTreeMap::from([(
            "grep".to_string(),
            ToolPolicy {
                forbidden: Some(vec!["**/secrets/**".to_string()]),
                ..Default::default()
            },
        )]),
        ..Policy::default()
    };
    let pe = PolicyEngine::from_effective_for_tests(effective);
    engine.set_policy(Arc::new(pe)).await;

    engine.start().await.unwrap();
    let resp = engine.process("find the secret").await.unwrap();

    assert_eq!(
        resp.tool_results.len(),
        1,
        "the scripted provider must produce exactly one tool result; \
         got {} results",
        resp.tool_results.len(),
    );
    let json_str = match &resp.tool_results[0] {
        ToolResult::Success(v) => v.to_string(),
        other => panic!("expected grep success, got {other:?}"),
    };
    let v: serde_json::Value = serde_json::from_str(&json_str).expect("grep output is JSON");
    let results = v
        .get("results")
        .and_then(|x| x.as_array())
        .expect("grep output has `results` array");
    for hit in results {
        let file = hit.get("file").and_then(|x| x.as_str()).unwrap_or("");
        let text = hit.get("text").and_then(|x| x.as_str()).unwrap_or("");
        assert!(
            !file.contains("secrets"),
            "walk must not read files under `forbidden`; got hit in {file:?}",
        );
        assert!(
            !text.contains("SECRET_MARKER"),
            "forbidden content leaked through the walk; got {text:?}",
        );
    }
    assert!(
        results.is_empty(),
        "no match expected: `readme.txt` does not contain the pattern \
         and `secrets/creds.txt` must be skipped; got {results:?}",
    );
}
