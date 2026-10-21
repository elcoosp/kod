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
