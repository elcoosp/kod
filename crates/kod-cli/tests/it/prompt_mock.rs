//! `run_prompt_inner` end-to-end over a `MockProvider`.
use kod_cli::commands::EngineBootstrapOptions;
use kod_cli::commands::prompt::run_prompt_inner;
use kod_config::KodConfig;
use kod_provider::testkit::{MockProvider, Script};
use kod_provider::{ModelRef, ProviderCapabilities, ProviderRegistry};
use std::sync::Arc;

fn config_pointing_at_mock() -> KodConfig {
    let mut cfg = KodConfig::default();
    let ep = cfg.llm.default_endpoint_mut();
    ep.model = "mock-model".to_string();
    ep.context_window = 32768;
    cfg
}

fn mock_registry(reply: &str) -> (Arc<ProviderRegistry>, ModelRef) {
    let provider = Arc::new(MockProvider::scripted(
        "mock",
        vec![Script::Text(reply.to_string())],
    ));
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
async fn prompt_returns_the_mock_reply() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config = config_pointing_at_mock();
    let (registry, model) = mock_registry("hello from the mock");
    let opts = EngineBootstrapOptions {
        db_path: Some(tmp.path().join("t.redb")),
        install_mcp: false,
        install_embedder: false,
        install_policy: false,
        install_secret_vault: false,
        provider_override: Some((registry, model, None)),
        ..Default::default()
    };
    let text = run_prompt_inner("ignored prompt".to_string(), &config, opts, true)
        .await
        .expect("run_prompt_inner");
    assert_eq!(text.trim(), "hello from the mock");
}

#[tokio::test]
async fn empty_prompt_is_rejected() {
    let config = KodConfig::default();
    let (registry, model) = mock_registry("unused");
    let opts = EngineBootstrapOptions {
        db_path: Some(std::env::temp_dir().join("kod-seam-unused.redb")),
        install_mcp: false,
        install_embedder: false,
        install_policy: false,
        install_secret_vault: false,
        provider_override: Some((registry, model, None)),
        ..Default::default()
    };
    let err = run_prompt_inner("   ".to_string(), &config, opts, true)
        .await
        .expect_err("empty prompt must be an error");
    assert!(err.to_string().contains("empty"), "unexpected: {err}");
}
