//! `run_agent_inner` end-to-end over a `MockProvider`.
//!
//! Same seam as `run_prompt_inner`: the wrapper reads `KodConfig`
//! and prints; the inner takes the config and bootstrap options and
//! returns the text. A `MockProvider` installed via
//! `EngineBootstrapOptions::provider_override` replaces the real
//! provider registry, so the run never touches the network.
use std::sync::Arc;

use kod_cli::commands::EngineBootstrapOptions;
use kod_cli::commands::chat::run_agent_inner;
use kod_config::KodConfig;
use kod_provider::testkit::{MockProvider, Script};
use kod_provider::{ModelRef, ProviderCapabilities, ProviderRegistry};

fn config_pointing_at_mock() -> KodConfig {
    let mut cfg = KodConfig::default();
    let ep = cfg.llm.default_endpoint_mut();
    ep.model = "mock-model".to_string();
    ep.context_window = 32768;
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

/// A single round with text: the inner returns `Some(text)`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_returns_the_mock_reply() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config = config_pointing_at_mock();
    let (registry, model) = mock_registry(vec![Script::Text("hello from the mock".to_string())]);

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

    let text = run_agent_inner("ignored goal".to_string(), &config, opts)
        .await
        .expect("run_agent_inner");
    assert_eq!(
        text.as_deref().map(str::trim),
        Some("hello from the mock"),
        "the mock reply should reach the caller",
    );
}

/// **Characterization.** An empty text reply is `Ok(Some(""))`, not
/// `Ok(None)`.
///
/// The engine maps a provider's empty text to `Some("")` rather than
/// `None`: `TaskResponse::text` is `Some` for a Text variant even when
/// the string is empty, and `None` only for a tool-only turn that
/// produced no final text. So `run_agent`'s wrapper prints a bare
/// `Agent <name>: ` line when the model replies with empty text —
/// pre-existing behaviour, pinned here so a change in either the
/// engine's mapping or the wrapper's `if let Some(text)` guard is
/// visible. A follow-up could suppress the print when the text is
/// empty; that would be a separate, deliberate change.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_empty_reply_is_some_empty_string() {
    let tmp = tempfile::TempDir::new().unwrap();
    let config = config_pointing_at_mock();
    let (registry, model) = mock_registry(vec![Script::Text(String::new())]);

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

    let text = run_agent_inner("ignored goal".to_string(), &config, opts)
        .await
        .expect("run_agent_inner");
    assert_eq!(
        text.as_deref(),
        Some(""),
        "an empty reply surfaces as Some(\"\") through the engine;          if this changes to None, run_agent's wrapper guard changes \
         behaviour (it prints 'Agent <name>: ' today)",
    );
}
