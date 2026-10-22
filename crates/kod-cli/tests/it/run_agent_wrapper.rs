//! `run_agent` wrapper end-to-end against a mock chat-completions
//! endpoint.
//!
//! `run_agent_inner` is already tested with `MockProvider`
//! (`agent_mock.rs`); the wrapper is not, because it builds
//! `EngineBootstrapOptions` internally with no `provider_override`.
//! Same shape as `run_prompt` (`kod_prompt_mock.rs`). The `name`
//! argument only affects the transcript key and a printed prefix, both
//! of which the mock ignores.
//!
//! `run_agent`'s fourth argument is `cli_preset: Option<String>`, not
//! a sandbox flag — `require_sandbox: false` is hardcoded in the
//! wrapper because `kod agent` has no interactive approval UI. So
//! there is no sandbox-failure case to test here; the empty-goal
//! guard lives in `run_agent_remote` (the socket client), not in
//! `run_agent` or `run_agent_inner`, so that case isn't tested either.
//!
//! Env redirections: `KOD_CONFIG_DIR` (config load), `HOME` (the
//! wrapper passes `db_path: None`, so the engine opens
//! `~/.kod/data/kod.redb` unless `HOME` is redirected),
//! `KOD_SESSIONS_DIR` (session recorder).
use crate::common::mock_llm::MockLlm;
use kod_cli::commands::chat::run_agent;
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

/// The wrapper: config load, opts build, `run_agent_inner`, print.
/// The hit counter is the load-bearing assertion — zero hits means
/// the config fell back to a default endpoint rather than the mock.
#[serial]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_agent_hits_the_mock_endpoint() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mock = MockLlm::start("agent reply from the mock").await;
    mock.write_config(tmp.path(), "");
    let _home = EnvGuard::set("HOME", tmp.path());
    let _cfg = EnvGuard::set("KOD_CONFIG_DIR", tmp.path());
    let _sess = EnvGuard::set("KOD_SESSIONS_DIR", tmp.path());

    run_agent(
        "test-agent".to_string(),
        "do the thing".to_string(),
        None,
        None,
    )
    .await
    .expect("run_agent against the mock must be Ok");

    assert!(
        mock.hits() >= 1,
        "the mock's /v1/chat/completions handler must have run — \
         zero hits means the config fell back to a default endpoint",
    );
}
