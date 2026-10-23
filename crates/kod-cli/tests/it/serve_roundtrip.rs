//! `run_serve(false, ...)` end-to-end: start the daemon, connect to
//! its socket, send a stop, join the task.
//!
//! This is the only remaining entry point that exercises
//! `engine_from_config` with the production install flags —
//! `install_mcp: true` (empty loop on a bare config: `McpConfig::default()`
//! has zero servers), `install_embedder: true` (`EmbeddingEndpoint::None`
//! is the default, so `from_config` returns `None` immediately), and
//! `install_policy: true` (reads a policy from cwd and the config file,
//! writes nothing).
//!
//! # Why the config.toml is written first
//!
//! `run_serve` calls `config.memory_db_path()`, which falls back to
//! `~/.kod/data/kod.redb` under `MemoryScope::Global`. That would write
//! into the developer's home. The `[memory] long_term_db_path` key is
//! the seam the rest of the test suite already uses (see
//! `memory_manager_paths`); setting it to a path inside the tempdir
//! makes `memory_db_path()` return early and never reach `home_dir()`.
#![cfg(unix)]

use kod_cli::commands::admin::run_serve;
use kod_core_serve::serve::stop_daemon;
use serial_test::serial;
use std::time::{Duration, Instant};

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

/// Write a minimal config that redirects the memory DB into the
/// tempdir. Without this, `run_serve` would open `~/.kod/data/kod.redb`.
fn write_config(cfg_dir: &std::path::Path, db_path: &std::path::Path) {
    std::fs::create_dir_all(cfg_dir).expect("create config dir");
    let db = db_path.to_str().expect("utf8 db path");
    std::fs::write(
        cfg_dir.join("config.toml"),
        format!("[memory]\nlong_term_db_path = \"{db}\"\n"),
    )
    .expect("write config.toml");
}

/// `kod serve` (foreground) starts, binds its socket, and stops on
/// `stop_daemon`. The assertions are the round trip: the socket appears
/// (proves `engine.start()` + `serve()` reached the accept loop), and
/// `stop_daemon` returns `Ok` and lets the spawned task complete
/// (proves the graceful-shutdown path).
///
/// The DB file must exist inside the tempdir afterwards — the
/// assertion pins the seam: if `long_term_db_path` had been ignored,
/// the file would be under `~/.kod/data/` and this assertion would
/// fail before the test could silently pollute home.
#[serial]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn serve_starts_and_stops() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("kod.redb");
    write_config(tmp.path(), &db_path);

    let _cfg = EnvGuard::set("KOD_CONFIG_DIR", tmp.path());
    let _sess = EnvGuard::set("KOD_SESSIONS_DIR", tmp.path());
    let _ckpt = EnvGuard::set("KOD_CHECKPOINT_DIR", tmp.path());

    let sock_path = tmp.path().join("kod.sock");
    let sock_for_task = sock_path.clone();
    let handle = tokio::spawn(async move { run_serve(false, Some(sock_for_task)).await });

    // Poll for the socket file. `run_serve` prints "Starting daemon at
    // …" after `engine.start()` and just before `serve()`, so the file
    // appearing is the first moment the accept loop is live.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut bound = false;
    while Instant::now() < deadline {
        if sock_path.exists() {
            bound = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    if !bound {
        handle.abort();
        panic!(
            "daemon did not bind the socket within 10s at {}",
            sock_path.display(),
        );
    }

    // Send the shutdown frame. `stop_daemon` writes the request and
    // waits up to 2s for a reply; it returns Ok once the frame is
    // written.
    //
    // Retry on connection failure. The poll above only proves that
    // the socket FILE exists; on macOS there is a window between
    // `UnixListener::bind` creating the file and the daemon's accept
    // loop being polled, during which `UnixStream::connect` fails.
    // Locally the window is microseconds; on a loaded macOS CI runner
    // it is observable -- the test failed at `stop_daemon` after
    // 0.34s with `connection refused` (see the CI log at 6c049b21).
    //
    // `stop_daemon` returns Err only on the connect (the code path
    // is: connect -> write -> flush -> bounded read; the read path
    // returns Ok even on timeout). So retrying is safe: a failed
    // attempt wrote nothing, and a later attempt that succeeds sends
    // the shutdown exactly once.
    let mut last_err = None;
    let mut stopped = false;
    for _ in 0..100 {
        match stop_daemon(&sock_path).await {
            Ok(()) => {
                stopped = true;
                break;
            }
            Err(e) => {
                last_err = Some(e);
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
    if !stopped {
        handle.abort();
        let msg = last_err
            .map(|e| e.to_string())
            .unwrap_or_else(|| "(no error recorded)".to_string());
        panic!(
            "stop_daemon must succeed against a live daemon; \
             gave up after 5s of retries. last error: {msg}",
        );
    }

    // The spawned task must now return. 15s covers a slow CI runner
    // whose CPU is busy with other jobs.
    let joined = tokio::time::timeout(Duration::from_secs(15), handle)
        .await
        .expect("run_serve must return within 15s of stop_daemon");
    let inner = joined.expect("the serve task must not panic");
    inner.expect("run_serve must return Ok on clean shutdown");

    // The DB was created where the config told it to go, not under
    // home. This is the assertion that pins `long_term_db_path`.
    assert!(
        db_path.exists(),
        "the daemon must have created its DB at the tempdir path \
         (`{}`); a missing file means `long_term_db_path` was ignored",
        db_path.display(),
    );
}
