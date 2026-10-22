//! `kod models` against a real HTTP endpoint, served by a loopback
//! axum mock.
//!
//! `run_models` calls `kod_core::build_registry`, which builds a real
//! `OpenAICompatible` provider from the config's `base_url`, then calls
//! `provider.list_models()` — an HTTP `GET` to that base_url. The only
//! hermetic way to exercise the success path is to point `base_url` at
//! a socket we control. `axum` is already a dev-dependency of
//! `kod-cli` (used by `tui_e2e::mock_llm`); this file stands up a
//! minimal equivalent with its own router so it does not depend on
//! the `#[cfg(unix)]` TUI test module.
//!
//! `KOD_CONFIG_DIR` points the config load at a temp dir, and the
//! temp `config.toml` sets `base_url` to the mock's `127.0.0.1`
//! address. `normalize_base_url` in the OpenAI provider appends `/v1`
//! when absent, so the mock serves both `/v1/models` and `/models` to
//! be robust to whichever the provider requests.
use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::response::Json;
use axum::routing::get;
use kod_cli::commands::admin::run_models;
use serde_json::{Value, json};
use serial_test::serial;

struct ConfigDirEnv(Option<std::ffi::OsString>);

impl ConfigDirEnv {
    fn set(dir: &std::path::Path) -> Self {
        let prev = std::env::var_os("KOD_CONFIG_DIR");
        // Safety: `#[serial]` on every caller ensures exclusivity.
        unsafe {
            std::env::set_var("KOD_CONFIG_DIR", dir);
        }
        ConfigDirEnv(prev)
    }
}

impl Drop for ConfigDirEnv {
    fn drop(&mut self) {
        // Safety: same invariant as `set`.
        unsafe {
            match &self.0 {
                Some(prev) => std::env::set_var("KOD_CONFIG_DIR", prev),
                None => std::env::remove_var("KOD_CONFIG_DIR"),
            }
        }
    }
}

/// The OpenAI `/v1/models` response shape: an object with a `data`
/// array of model entries. Each entry is `{"id": ..., "object":
/// "model", ...}`. `list_models` in the provider parses this.
async fn list_models_handler(State(models): State<Arc<Vec<String>>>) -> Json<Value> {
    let data: Vec<Value> = models
        .iter()
        .map(|id| json!({"id": id, "object": "model", "created": 0, "owned_by": "mock"}))
        .collect();
    Json(json!({"object": "list", "data": data}))
}

/// Start the mock on an ephemeral port and return its `base_url`.
/// The listener stays alive because the spawned axum server holds it.
async fn spawn_mock(models: Vec<String>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let router = Router::new()
        .route("/v1/models", get(list_models_handler))
        .route("/models", get(list_models_handler))
        .with_state(Arc::new(models));
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    format!("http://{}", addr)
}

/// A config.toml pointing at `base_url` with `model = "test-model"`.
fn config_toml(base_url: &str) -> String {
    format!(
        r#"[llm]
network_access = false

[[llm.endpoints]]
name = "mock"
provider = "openai_compatible"
base_url = "{base_url}"
model = "test-model"
max_tokens = 128
context_window = 8192
"#
    )
}

/// The success path: `kod models` lists what the endpoint reports.
#[serial]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn models_against_a_mock_endpoint_succeeds() {
    let base_url = spawn_mock(vec!["test-model".to_string(), "other-model".to_string()]).await;

    let cfg_dir = tempfile::TempDir::new().unwrap();
    std::fs::write(cfg_dir.path().join("config.toml"), config_toml(&base_url)).unwrap();
    let _guard = ConfigDirEnv::set(cfg_dir.path());

    run_models(None)
        .await
        .expect("the mock endpoint reports models, so run_models must succeed");
}

/// The filter path: a filter that matches one of the mock's models
/// still returns Ok — the filter only narrows `shown`, never the
/// models list itself.
#[serial]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn models_with_a_matching_filter_succeeds() {
    let base_url = spawn_mock(vec!["test-model".to_string(), "other-model".to_string()]).await;

    let cfg_dir = tempfile::TempDir::new().unwrap();
    std::fs::write(cfg_dir.path().join("config.toml"), config_toml(&base_url)).unwrap();
    let _guard = ConfigDirEnv::set(cfg_dir.path());

    run_models(Some("test".to_string()))
        .await
        .expect("a matching filter narrows the display but keeps the run Ok");
}

/// A filter matching nothing is still Ok — `run_models` prints "No
/// model matches" and returns Ok. That is the branch a user hits when
/// they mistype a model name; it must not be an error.
#[serial]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn models_with_a_non_matching_filter_is_still_ok() {
    let base_url = spawn_mock(vec!["test-model".to_string()]).await;

    let cfg_dir = tempfile::TempDir::new().unwrap();
    std::fs::write(cfg_dir.path().join("config.toml"), config_toml(&base_url)).unwrap();
    let _guard = ConfigDirEnv::set(cfg_dir.path());

    run_models(Some("does-not-exist".to_string()))
        .await
        .expect("a filter that matches nothing must not be an error");
}

/// An endpoint that reports no models is also Ok — `run_models`
/// prints "The provider is reachable but reports no models." and
/// suggests a pull command. That is the empty-list branch.
#[serial]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn models_against_an_empty_endpoint_is_ok() {
    let base_url = spawn_mock(vec![]).await;

    let cfg_dir = tempfile::TempDir::new().unwrap();
    std::fs::write(cfg_dir.path().join("config.toml"), config_toml(&base_url)).unwrap();
    let _guard = ConfigDirEnv::set(cfg_dir.path());

    run_models(None)
        .await
        .expect("an endpoint that lists no models must not be an error");
}
