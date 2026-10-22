//! `run_swarm` wrapper end-to-end against a scripted mock endpoint.
//!
//! `run_swarm_inner` is already tested with `MockProvider`
//! (`swarm_cli_mock.rs`); the wrapper is not, because it builds
//! `EngineBootstrapOptions` internally with no `provider_override`.
//! The only way to exercise it is through the config's `base_url`.
//!
//! The swarm makes two completion calls for a single-subtask goal:
//! one for decompose (non-streaming, expecting a JSON subtask array),
//! one per spawned agent (streaming). Both are served by the same
//! `MockLlm` — it branches on the request's `stream` field.
use crate::common::mock_llm::MockLlm;
use kod_cli::commands::swarm::run_swarm;
use serial_test::serial;

struct EnvGuard {
    key: &'static str,
    prev: Option<std::ffi::OsString>,
}

impl EnvGuard {
    fn set(key: &'static str, value: &std::path::Path) -> Self {
        let prev = std::env::var_os(key);
        // Safety: `#[serial]` on every caller ensures exclusivity.
        unsafe {
            std::env::set_var(key, value);
        }
        EnvGuard { key, prev }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // Safety: same invariant as `set`.
        unsafe {
            match &self.prev {
                Some(prev) => std::env::set_var(self.key, prev),
                None => std::env::remove_var(self.key),
            }
        }
    }
}

/// Build a `KodConfig` in Rust and save it as TOML, so the
/// `Isolation::Shared` variant's serde representation (which this
/// test does not want to hardcode) is produced by serde, not guessed.
fn write_swarm_config(cfg_dir: &std::path::Path, base_url: &str) {
    use kod_config::{Isolation, KodConfig, ProviderKind};
    std::fs::create_dir_all(cfg_dir).expect("create config dir");
    let mut cfg = KodConfig::default();
    let ep = cfg.llm.default_endpoint_mut();
    ep.base_url = base_url.to_string();
    ep.provider = ProviderKind::OpenAICompatible;
    ep.model = "test-model".to_string();
    // The swarm's system prompt + tool set is ~9700 chars; the
    // engine refuses a request whose prompt exceeds the endpoint's
    // context_window. 32768 matches what `swarm_cli_mock.rs` and
    // `agent_mock.rs` use for the same reason — the default
    // `KodConfig` window is too small to hold the tool schemas.
    ep.context_window = 32768;
    cfg.swarm.max_agents = 2;
    cfg.swarm.merge_results = false;
    cfg.swarm.isolation = Isolation::Shared;
    cfg.save_to(&cfg_dir.join("config.toml"))
        .expect("save config.toml");
}

/// One-subtask goal, `merge_results = false`: the wrapper loads the
/// config, builds the engine, calls `run_swarm_inner` (decompose ->
/// one agent), prints the response, and returns `Ok(())`. Both the
/// wrapper and `run_swarm_inner` are exercised end to end.
#[serial]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_swarm_hits_the_mock_endpoint() {
    let tmp = tempfile::TempDir::new().unwrap();
    let decompose = r#"[{"name":"alpha","description":"do the thing","expected_writes":[],"capability":"coding","depends_on":[]}]"#;
    let mock = MockLlm::start_scripted(vec![
        decompose.to_string(),
        "agent said done".to_string(),
    ])
    .await;
    write_swarm_config(tmp.path(), &mock.base_url);
    let _home = EnvGuard::set("HOME", tmp.path());
    let _cfg = EnvGuard::set("KOD_CONFIG_DIR", tmp.path());
    let _sess = EnvGuard::set("KOD_SESSIONS_DIR", tmp.path());

    run_swarm("do the thing".to_string(), None, None, false)
        .await
        .expect("run_swarm against the mock must be Ok");

    let hits = mock.hits();
    assert!(
        hits >= 2,
        "decompose (1) + at least one agent (1) = at least 2 completion \
         requests, got {hits} — fewer means the config fell back to a \
         default endpoint",
    );
}
