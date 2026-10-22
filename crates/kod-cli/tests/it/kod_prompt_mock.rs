//! `run_prompt` and `run_streaming_prompt` end-to-end against a mock
//! chat-completions endpoint (`common::mock_llm`).
//!
//! The wrappers build `EngineBootstrapOptions` internally with no
//! `provider_override`, so the only way to point them at a mock is
//! through the config's `base_url`. The `_inner` functions are already
//! tested with `MockProvider` (`prompt_mock.rs`); this file covers
//! the wrappers.
//!
//! Env redirections:
//! - `KOD_CONFIG_DIR` — the wrapper reads `KodConfig::load_default()`
//! - `HOME` — the wrappers pass `db_path: None`, so the engine opens
//!   `~/.kod/data/kod.redb` unless `HOME` is redirected. No config
//!   key intercepts that path.
//! - `KOD_SESSIONS_DIR` — the session recorder writes elsewhere
use crate::common::mock_llm::MockLlm;
use kod_cli::commands::prompt::{run_prompt, run_streaming_prompt};
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

// ---- run_prompt ----------------------------------------------------

#[serial]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_prompt_hits_the_mock_endpoint() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mock = MockLlm::start("hello from the mock").await;
    mock.write_config(tmp.path(), "");
    let _home = EnvGuard::set("HOME", tmp.path());
    let _cfg = EnvGuard::set("KOD_CONFIG_DIR", tmp.path());
    let _sess = EnvGuard::set("KOD_SESSIONS_DIR", tmp.path());

    run_prompt("hi".to_string(), None, true, false)
        .await
        .expect("run_prompt against the mock must be Ok");

    assert!(
        mock.hits() >= 1,
        "the mock's /v1/chat/completions handler must have run — \
         zero hits means the config fell back to a default endpoint",
    );
}

#[serial]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_prompt_rejects_empty_input() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mock = MockLlm::start("should not be reached").await;
    mock.write_config(tmp.path(), "");
    let _home = EnvGuard::set("HOME", tmp.path());
    let _cfg = EnvGuard::set("KOD_CONFIG_DIR", tmp.path());
    let _sess = EnvGuard::set("KOD_SESSIONS_DIR", tmp.path());

    let err = run_prompt("   ".to_string(), None, true, false)
        .await
        .expect_err("an empty prompt must be an error");
    assert!(
        err.to_string().contains("empty prompt"),
        "unexpected error: {err}",
    );
    assert_eq!(
        mock.hits(),
        0,
        "the empty-prompt guard must fire before any HTTP request",
    );
}

// ---- run_streaming_prompt ------------------------------------------

#[serial]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_streaming_prompt_hits_the_mock_endpoint() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mock = MockLlm::start("streaming hello").await;
    mock.write_config(tmp.path(), "");
    let _home = EnvGuard::set("HOME", tmp.path());
    let _cfg = EnvGuard::set("KOD_CONFIG_DIR", tmp.path());
    let _sess = EnvGuard::set("KOD_SESSIONS_DIR", tmp.path());

    run_streaming_prompt("hi".to_string(), None)
        .await
        .expect("run_streaming_prompt against the mock must be Ok");

    assert!(
        mock.hits() >= 1,
        "the mock's /v1/chat/completions handler must have run",
    );
}

#[serial]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_streaming_prompt_rejects_empty_input() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mock = MockLlm::start("should not be reached").await;
    mock.write_config(tmp.path(), "");
    let _home = EnvGuard::set("HOME", tmp.path());
    let _cfg = EnvGuard::set("KOD_CONFIG_DIR", tmp.path());
    let _sess = EnvGuard::set("KOD_SESSIONS_DIR", tmp.path());

    let err = run_streaming_prompt("   ".to_string(), None)
        .await
        .expect_err("an empty prompt must be an error");
    assert!(
        err.to_string().contains("empty prompt"),
        "unexpected error: {err}",
    );
    assert_eq!(
        mock.hits(),
        0,
        "the empty-prompt guard must fire before any HTTP request",
    );
}
