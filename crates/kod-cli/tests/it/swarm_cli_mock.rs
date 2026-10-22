//! `run_swarm_inner` end-to-end over a `MockProvider`.
use std::sync::Arc;

use kod_cli::commands::EngineBootstrapOptions;
use kod_cli::commands::swarm::run_swarm_inner;
use kod_config::{Isolation, KodConfig, SwarmConfig};
use kod_provider::testkit::{MockProvider, Script};
use kod_provider::{ModelRef, ProviderCapabilities, ProviderRegistry};

const ONE_SUBTASK: &str = r#"[{"name":"alpha","description":"do the thing","expected_writes":[],"capability":"coding","depends_on":[]}]"#;

fn config_pointing_at_mock() -> KodConfig {
    let mut cfg = KodConfig::default();
    let ep = cfg.llm.default_endpoint_mut();
    ep.model = "mock-model".to_string();
    ep.context_window = 32768;
    cfg.swarm = SwarmConfig {
        max_agents: 2,
        merge_results: false,
        isolation: Isolation::Shared,
        ..Default::default()
    };
    cfg
}

fn mock_registry(script: Vec<Script>) -> (Arc<ProviderRegistry>, ModelRef) {
    let provider = Arc::new(MockProvider::scripted("mock", script));
    let mut registry = ProviderRegistry::new();
    registry.insert(
        "mock",
        provider,
        ProviderCapabilities::conservative(),
        "mock-model",
    );
    (Arc::new(registry), ModelRef::new("mock", "mock-model"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn swarm_run_returns_the_concatenated_result() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config = config_pointing_at_mock();
    let script = vec![
        Script::Text(ONE_SUBTASK.to_string()),
        Script::Text("agent said done".to_string()),
    ];
    let (registry, model) = mock_registry(script);

    let opts = EngineBootstrapOptions {
        db_path: Some(tmp.path().join("t.redb")),
        install_mcp: false,
        install_embedder: false,
        install_policy: false,
        install_secret_vault: false,
        install_skills: false,
        provider_override: Some((registry, model, None)),
        ..Default::default()
    };

    let resp = run_swarm_inner("do the thing".to_string(), None, false, &config, opts)
        .await
        .expect("run_swarm_inner");

    assert!(
        !resp.merged_by_model,
        "merge_results=false must not synthesize"
    );
    assert!(
        resp.merged.contains("agent said done"),
        "concatenated merged text lost the agent reply: {:?}",
        resp.merged,
    );
    assert_eq!(resp.subtasks.len(), 1);
    assert!(resp.conflicts.is_empty());
}

const TWO_SUBTASKS: &str = r#"[{"name":"alpha","description":"do a","expected_writes":[],"capability":"coding","depends_on":[]},{"name":"beta","description":"do b","expected_writes":[],"capability":"coding","depends_on":[]}]"#;

/// `merge_results = true` with a synthesis response as the third
/// script entry: after the single agent finishes, the runner asks the
/// model to synthesize, and `merged_by_model` reflects that.
///
/// The `merge` *parameter* to `run_swarm_inner` is only the label
/// printed at the top of the run; the synthesis decision is
/// `config.swarm.merge_results`. Setting the parameter without the
/// config would produce the concatenation path with a "merge on"
/// header — which is why the config is what this test drives.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn swarm_merge_results_true_uses_a_synthesis_call() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut config = config_pointing_at_mock();
    config.swarm.merge_results = true;
    let script = vec![
        Script::Text(ONE_SUBTASK.to_string()),
        Script::Text("agent did the thing".to_string()),
        Script::Text("synthesized summary".to_string()),
    ];
    let (registry, model) = mock_registry(script);

    let opts = EngineBootstrapOptions {
        db_path: Some(tmp.path().join("t.redb")),
        install_mcp: false,
        install_embedder: false,
        install_policy: false,
        install_secret_vault: false,
        install_skills: false,
        provider_override: Some((registry, model, None)),
        ..Default::default()
    };

    let resp = run_swarm_inner("do the thing".to_string(), None, false, &config, opts)
        .await
        .expect("run_swarm_inner with merge_results = true");

    assert!(
        resp.merged_by_model,
        "merge_results = true must ask the model to synthesize",
    );
    assert!(
        resp.merged.contains("synthesized summary"),
        "merged text should be the synthesis reply, got {:?}",
        resp.merged,
    );
}

/// Two subtasks, `max_agents = 2`: the runner fans out one agent per
/// subtask. The per-agent results and the merged text both carry
/// both replies, and the subtask count reflects the decompose
/// response — not the cap.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn swarm_with_two_subtasks_fans_out() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config = config_pointing_at_mock();
    let script = vec![
        Script::Text(TWO_SUBTASKS.to_string()),
        Script::Text("alpha reply".to_string()),
        Script::Text("beta reply".to_string()),
    ];
    let (registry, model) = mock_registry(script);

    let opts = EngineBootstrapOptions {
        db_path: Some(tmp.path().join("t.redb")),
        install_mcp: false,
        install_embedder: false,
        install_policy: false,
        install_secret_vault: false,
        install_skills: false,
        provider_override: Some((registry, model, None)),
        ..Default::default()
    };

    let resp = run_swarm_inner("do the thing".to_string(), None, false, &config, opts)
        .await
        .expect("run_swarm_inner with two subtasks");

    assert_eq!(resp.subtasks.len(), 2, "decompose returned two subtasks");
    assert_eq!(
        resp.per_agent.len(),
        2,
        "each subtask must produce one agent result",
    );
    assert!(
        resp.merged.contains("alpha reply"),
        "alpha's reply lost from merged: {:?}",
        resp.merged,
    );
    assert!(
        resp.merged.contains("beta reply"),
        "beta's reply lost from merged: {:?}",
        resp.merged,
    );
}
