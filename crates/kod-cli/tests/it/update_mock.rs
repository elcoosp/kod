//! `kod update` against a loopback GitHub-releases mock.
//!
//! `run_update` GETs `https://api.github.com/repos/<repo>/releases/
//! latest`, reads the JSON body, and compares the release's tag to
//! the crate's current version. `KOD_UPDATE_URL` overrides the whole
//! endpoint, so the test serves a canned release body from a
//! `127.0.0.1` mock and never contacts GitHub.
//!
//! Same shape as `admin_models_mock` — a self-contained axum router
//! with a `with_state` handler, bound to an ephemeral port — so the
//! test does not depend on `tui_e2e::mock_llm` or any `#[cfg(unix)]`
//! module.
use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::response::Json;
use axum::routing::get;
use kod_cli::commands::admin::run_update;
use serde_json::{Value, json};
use serial_test::serial;

struct UpdateUrlEnv(Option<std::ffi::OsString>);

impl UpdateUrlEnv {
    fn set(url: &str) -> Self {
        let prev = std::env::var_os("KOD_UPDATE_URL");
        // Safety: `#[serial]` on every caller ensures exclusivity.
        unsafe {
            std::env::set_var("KOD_UPDATE_URL", url);
        }
        UpdateUrlEnv(prev)
    }
}

impl Drop for UpdateUrlEnv {
    fn drop(&mut self) {
        // Safety: same invariant as `set`.
        unsafe {
            match &self.0 {
                Some(prev) => std::env::set_var("KOD_UPDATE_URL", prev),
                None => std::env::remove_var("KOD_UPDATE_URL"),
            }
        }
    }
}

/// Serve a canned GitHub-release JSON body on an ephemeral port.
/// `body` is the full object the API returns; a real response carries
/// dozens of fields, but `run_update` reads only `tag_name`.
async fn spawn_release_mock(body: Value) -> String {
    async fn handler(State(body): State<Arc<Value>>) -> Json<Value> {
        Json((*body).clone())
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral");
    let addr = listener.local_addr().expect("local addr");
    let router = Router::new()
        .route("/releases/latest", get(handler))
        .with_state(Arc::new(body));
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    format!("http://{}/releases/latest", addr)
}

/// A release tag that is *newer* than the crate's current version. The
/// test reads `CARGO_PKG_VERSION` at compile time to construct a
/// deliberately-higher version so the "update available" branch fires
/// regardless of what this commit's version happens to be.
fn newer_tag() -> String {
    let v = env!("CARGO_PKG_VERSION");
    let mut parts: Vec<u32> = v.split('.').map(|p| p.parse().unwrap_or(0)).collect();
    while parts.len() < 3 {
        parts.push(0);
    }
    parts[0] += 1;
    format!("v{}.{}.{}", parts[0], parts[1], parts[2])
}

/// A newer release is reported and `run_update` returns Ok. The body
/// shape is the minimum `run_update` reads: `tag_name` plus the URL it
/// prints. Asserting on the success shape rather than a specific
/// string; the print itself is not captured.
#[serial]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_with_newer_release_is_ok() {
    let tag = newer_tag();
    let url = spawn_release_mock(json!({
        "tag_name": tag,
        "html_url": "https://github.com/elcoosp/kod/releases",
        "name": tag,
    }))
    .await;
    let _guard = UpdateUrlEnv::set(&url);
    run_update()
        .await
        .expect("a newer release is a normal 'update available' result");
}

/// A release tag equal to the current version. `run_update` reports
/// "already up to date" and returns Ok. A regression that treated
/// "same version" as an error would fail here.
#[serial]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_with_current_release_is_ok() {
    let v = env!("CARGO_PKG_VERSION");
    let url = spawn_release_mock(json!({
        "tag_name": format!("v{v}"),
        "html_url": "https://github.com/elcoosp/kod/releases",
        "name": format!("v{v}"),
    }))
    .await;
    let _guard = UpdateUrlEnv::set(&url);
    run_update()
        .await
        .expect("a current release is not an error");
}

/// An endpoint that returns a non-2xx status is an error — `run_update`
/// must return, not panic. The error message is the GitHub body's
/// short prefix, which the mock shapes here.
#[serial]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_with_server_error_is_err() {
    async fn handler() -> (axum::http::StatusCode, &'static str) {
        (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "boom")
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = Router::new().route("/releases/latest", get(handler));
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    let _guard = UpdateUrlEnv::set(&format!("http://{}/releases/latest", addr));
    let err = run_update()
        .await
        .expect_err("a 500 from the release endpoint must be an error");
    assert!(
        err.to_string().contains("fatal") || err.to_string().contains("admin"),
        "unexpected error: {err}",
    );
}
