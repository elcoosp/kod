//! Shared setup for the E2E TUI tests: a hermetic config + state
//! directory, and a helper that spawns the `kod tui` binary under a
//! `terminal-testlib` PTY.
//!
//! Three env vars make the spawned process hermetic:
//!
//! - `KOD_CONFIG_DIR` — the directory holding `config.toml`. Overrides
//!   the platform default (`dirs::config_dir()/kod`), so the test
//!   never reads the developer's real config.
//! - `KOD_TUI_STATE_DIR` — where the TUI writes
//!   `tui_session.json` / `tui_history.json`.
//! - `KOD_TEST_DB` — the redb memory database path. Without this, the
//!   spawned TUI opens `~/.kod/data/kod.redb` and races other tests
//!   for the lock.
//!
//! The mock LLM's port is written into `config.toml`, so the spawned
//! process reaches the mock without needing an env var for it.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tempfile::TempDir;
use terminal_testlib::TuiTestHarness;
use terminal_testlib::portable_pty::CommandBuilder;

use super::mock_llm::MockServer;

/// One isolated test environment.
pub struct TestEnv {
    pub temp: TempDir,
    pub config_dir: PathBuf,
    pub state_dir: PathBuf,
    pub db_path: PathBuf,
    pub mock: MockServer,
}

impl TestEnv {
    /// Create a tempdir layout and start a mock LLM. The config file
    /// is written here so the spawned process can read it at startup.
    pub fn new() -> Self {
        Self::new_with_reply(super::mock_llm::MOCK_REPLY)
    }

    /// Like [`new`], with a custom mock reply.
    pub fn new_with_reply(reply: &str) -> Self {
        let temp = TempDir::new().expect("tempdir");
        let config_dir = temp.path().join("config");
        let state_dir = temp.path().join("state");
        let db_path = temp.path().join("kod.redb");
        std::fs::create_dir_all(&config_dir).expect("config dir");
        std::fs::create_dir_all(&state_dir).expect("state dir");

        let mock = MockServer::start(reply);

        let env = TestEnv {
            temp,
            config_dir,
            state_dir,
            db_path,
            mock,
        };
        env.write_config();
        env
    }

    /// Write a minimal, hermetic `config.toml`.
    ///
    /// The endpoint shape mirrors `LlmConfig::default()` from
    /// `kod-config`, with `base_url` pointed at the mock and the
    /// model name swapped for the mock's advertised id. Memory is
    /// disabled to avoid any lingering redb state between tests that
    /// would otherwise slow the suite or trip a lock.
    fn write_config(&self) {
        let port = self.mock.port;
        let config = format!(
            r#"config_version = 2

[llm]
network_access = false

[[llm.endpoints]]
name = "default"
provider = "openai-compatible"
base_url = "http://127.0.0.1:{port}/v1"
model = "mock-model"
temperature = 0.0
max_tokens = 256
context_window = 8192
timeout_secs = 30

[memory]
enable_memory = false
enable_semantic_search = false

[jev]
enabled = false

[tools]
auto_check = false
auto_lsp = false
"#
        );
        std::fs::write(self.config_dir.join("config.toml"), config)
            .expect("write config.toml");
    }

    /// Build a `CommandBuilder` for the spawned `kod tui` process.
    pub fn command(&self) -> CommandBuilder {
        let exe = env!("CARGO_BIN_EXE_kod");
        let mut cmd = CommandBuilder::new(exe);
        // `--no-resume` avoids reading a stale session from the state
        // dir on the second run of a test — the dir is fresh per
        // test, but pinning the flag documents the intent.
        cmd.args(["tui", "--no-resume"]);
        cmd.env("KOD_CONFIG_DIR", self.config_dir.as_os_str());
        cmd.env("KOD_TUI_STATE_DIR", self.state_dir.as_os_str());
        cmd.env("KOD_TEST_DB", self.db_path.as_os_str());
        // The spawned process inherits the test's cwd, which is the
        // workspace root — deliberately, so `/map` and similar
        // commands have a real tree to look at. If a test needs a
        // different cwd it can override here.
        cmd
    }

    /// Spawn a harness at the given size.
    pub fn spawn(&self, width: u16, height: u16) -> TuiTestHarness {
        let mut harness = TuiTestHarness::builder()
            .with_size(width, height)
            // The TUI's first frame lands after the engine is built
            // (config + registry + hooks). On a cold build that is
            // sub-second, but under parallel test load give it head-
            // room so a slow CI machine does not produce a flaky
            // `Timeout`.
            .with_timeout(Duration::from_secs(15))
            .with_poll_interval(Duration::from_millis(50))
            .build()
            .expect("build harness");
        harness.spawn(self.command()).expect("spawn kod tui");
        harness
    }

    /// Path to the config file, for tests that want to inspect it.
    pub fn config_path(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }
}

/// Wait for the TUI's startup marker to appear in the screen.
///
/// The marker is the placeholder the chat widget renders when the
/// transcript is empty. Matching on it is the smallest reliable
/// signal that `init_engine` finished, the first frame was drawn, and
/// the input box is ready for keystrokes.
pub fn wait_for_ready(harness: &mut TuiTestHarness) {
    harness
        .wait_for_text("No messages yet")
        .expect("TUI did not reach the ready state — engine init likely failed");
}
