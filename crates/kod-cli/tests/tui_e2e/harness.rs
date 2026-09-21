//! E2E test harness for the KOD TUI.
//!
//! # Why this wraps `terminal-testlib`'s `ScreenState` instead of its
//! # `TuiTestHarness`
//!
//! `TuiTestHarness::read` is a single `read` on a freshly-cloned fd
//! per call, with a 100 ms timeout treated as "no data available"
//! (`TermTestError::Io(WouldBlock)` or a channel timeout → return
//! `Ok(0)`). `Ok(0)` on a blocking PTY read means EOF, not "nothing
//! yet" — but the caller (`update_state`) treats it as the latter and
//! breaks out of its drain loop. When the child's first frame arrives
//! in a burst larger than one buffer, most of it is dropped: the
//! spawned reader thread is left orphaned with a competing fd clone,
//! racing the next call's read on the same fd. Feeding the *same*
//! bytes to `ScreenState` directly parses them perfectly — the parser
//! is not the problem, the reader is.
//!
//! So this harness uses:
//!
//! - `portable-pty` for PTY allocation and process spawn (proven by
//!   the equivalent Python `pty.openpty()` path, which captured the
//!   full frame where the test harness did not).
//! - `terminal_testlib::ScreenState` for VT100 parsing (it handles
//!   the alt-screen entry, SGR colors, and the box-drawing frame
//!   correctly — verified byte-for-byte against a captured stream).
//! - `terminal_testlib::{KeyCode, Modifiers}` for input encoding,
//!   re-exported below so tests do not import `terminal-testlib`
//!   directly and can migrate when its reader is fixed.
//!
//! The reader is the standard shape: one background thread does
//! blocking reads forever into a `Vec<u8>` behind a `Mutex`, a
//! `Condvar` wakes pollers when bytes arrive, and the test thread
//! polls a parsed `ScreenState` that is rebuilt from the buffer.
//! That rebuild-on-read is cheap (`ScreenState::feed` on a few KB),
//! and it sidesteps the incremental-parser-vs-fresh-bytes confusion
//! entirely.

use std::io::Read;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use tempfile::TempDir;
use terminal_testlib::ScreenState;

pub use terminal_testlib::{KeyCode, Modifiers};

use super::mock_llm::MockServer;

/// One isolated test environment: a tempdir, a config pointing at a
/// loopback mock, and a running mock server.
pub struct TestEnv {
    /// Kept alive for the whole test; dropping it would delete the
    /// tempdir while the spawned TUI is still using it.
    pub _temp: TempDir,
    pub config_dir: PathBuf,
    pub state_dir: PathBuf,
    pub db_path: PathBuf,
    /// The directory the spawned TUI runs in.
    ///
    /// This exists because the engine builds a repository map on the
    /// first prompt by walking the process's cwd. A test that lets
    /// the child inherit the cargo runner's cwd (often `$HOME` or a
    /// checkout with a large `.git`/`node_modules`) blocks the
    /// prompt for as long as the walk takes — minutes on a real home
    /// directory, which manifests as a mysterious "connecting…"
    /// hang with zero network activity.
    ///
    /// Pointing cwd at the test's own tempdir makes the walk trivial
    /// (a handful of files) so the prompt pipeline reaches the
    /// provider in milliseconds. The same pattern applies to a
    /// developer running `kod tui` from `$HOME`: the walk is
    /// bounded by whatever is under cwd.
    pub workspace: PathBuf,
    pub mock: MockServer,
}

impl TestEnv {
    pub fn new() -> Self {
        Self::new_with_reply(super::mock_llm::MOCK_REPLY)
    }

    pub fn new_with_reply(reply: &str) -> Self {
        Self::new_full(reply, false)
    }

    pub fn new_byte_by_byte(reply: &str) -> Self {
        Self::new_full(reply, true)
    }

    fn new_full(reply: &str, byte_by_byte: bool) -> Self {
        let temp = TempDir::new().expect("tempdir");
        let config_dir = temp.path().join("config");
        let state_dir = temp.path().join("state");
        let workspace = temp.path().join("workspace");
        let db_path = temp.path().join("kod.redb");
        std::fs::create_dir_all(&config_dir).expect("config dir");
        std::fs::create_dir_all(&state_dir).expect("state dir");
        std::fs::create_dir_all(&workspace).expect("workspace dir");
        // Seed the workspace with a single Rust file so the repomap
        // has something to scan. Without any files the walk is
        // trivially empty, which is fine, but a single file
        // exercises the read-one-file path in the repomap builder.
        std::fs::write(
            workspace.join("main.rs"),
            "fn main() {}\n",
        )
        .expect("seed workspace file");

        let mock = if byte_by_byte {
            MockServer::start_byte_by_byte(reply)
        } else {
            MockServer::start(reply)
        };

        let env = TestEnv {
            _temp: temp,
            config_dir,
            state_dir,
            db_path,
            workspace,
            mock,
        };
        env.write_config();
        env
    }

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
        eprintln!("test: writing config.toml pointing at mock on port {}", port);
        std::fs::write(self.config_dir.join("config.toml"), config).expect("write config.toml");
    }

    /// Spawn the TUI in a PTY at the given size.
    ///
    /// The child runs under `sh -c 'stty …; exec kod …'` because on
    /// macOS the `PtySize` passed to `openpty` does not propagate to
    /// the slave fd the child holds. Setting the size from inside the
    /// child, before `exec`, is what makes `stty size` report the
    /// real dimensions and the first frame paint at full size.
    pub fn spawn(&self, width: u16, height: u16) -> TuiSession {
        let pty_system = portable_pty::native_pty_system();
        let pair = pty_system
            .openpty(portable_pty::PtySize {
                rows: height,
                cols: width,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");

        let log_file = self.state_dir.join("tui-stderr.log");
        let mut cmd = portable_pty::CommandBuilder::new("/bin/sh");
        cmd.arg("-c");
        // `sh -c 'script' name arg1 arg2 …` binds `$0` = name and
        // `$@` = [arg1, arg2, …]. The `"$@"` expansion is the
        // canonical way to pass argv through a shell without
        // re-parsing it.
        //
        // stderr goes to a file, not the PTY. The TUI paints the
        // alternate screen with cursor-positioned output; a
        // `tracing::warn!` on fd 2 — which is the same PTY slave —
        // writes its text at whatever cursor position the frame is
        // mid-way through and corrupts the render. The pre-`main.rs`
        // bug that the tracing init fixed also hid this: with no
        // subscriber installed, nothing was written to fd 2. The
        // file redirect keeps the log for the failure dump without
        // letting it touch the screen.
        cmd.arg(format!(
            "stty rows {height} cols {width} 2>/dev/null; exec \"$@\" 2>{}",
            log_file.display(),
        ));
        // `$0` for the shell — any string; not the program path.
        cmd.arg("kod-tui-wrapper");
        cmd.arg(env!("CARGO_BIN_EXE_kod"));
        cmd.arg("tui");
        cmd.arg("--no-resume");
        let stderr_log = log_file.clone();
        eprintln!(
            "test: spawning with KOD_CONFIG_DIR={}",
            self.config_dir.display()
        );
        // `cwd` decides the workspace the repomap builder walks on
        // the first prompt. See `TestEnv::workspace` for why this
        // must not be inherited.
        cmd.cwd(&self.workspace);
        cmd.env("KOD_CONFIG_DIR", self.config_dir.as_os_str());
        cmd.env("KOD_TUI_STATE_DIR", self.state_dir.as_os_str());
        cmd.env("KOD_TEST_DB", self.db_path.as_os_str());
        cmd.env("TERM", "xterm-256color");
        // Route tracing to a file next to the state so a failing test
        // can read the spawned process's own view of the request
        // without polluting the PTY. `RUST_LOG` is honored when the
        // test sets `KOD_TUI_LOG`; otherwise it stays quiet.
        if let Ok(level) = std::env::var("KOD_TUI_LOG") {
            let log_path = self.state_dir.join("tui.log");
            cmd.env("RUST_LOG", level);
            // tracing_subscriber::fmt with a file writer is not
            // wired into kod's main; the closest we get is stderr.
            // Redirect stderr to a file at the shell level.
            let _ = log_path; // reserved for a future wiring
        }

        let child = pair.slave.spawn_command(cmd).expect("spawn kod tui");
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader().expect("clone reader");
        let writer = pair.master.take_writer().expect("take writer");

        // Reader thread: reads forever, appends to a shared buffer,
        // wakes any waiters. Exits on EOF or error.
        let buf = Arc::new((Mutex::new(Vec::<u8>::new()), Condvar::new()));
        let buf_r = buf.clone();
        let reader_thread = std::thread::spawn(move || {
            let mut chunk = [0u8; 4096];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        let (lock, cvar) = &*buf_r;
                        let mut g = lock.lock().unwrap();
                        g.extend_from_slice(&chunk[..n]);
                        cvar.notify_all();
                    }
                    Err(_) => break,
                }
            }
        });

        TuiSession {
            width,
            height,
            stderr_log: Some(stderr_log),
            buf,
            writer: Mutex::new(writer),
            child: Mutex::new(child),
            reader_thread: Some(reader_thread),
        }
    }

    /// Path to the config file, for tests that want to inspect it.
    #[allow(dead_code)]
    pub fn config_path(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }
}

/// A running TUI session in a PTY.
pub struct TuiSession {
    width: u16,
    height: u16,
    stderr_log: Option<PathBuf>,
    buf: Arc<(Mutex<Vec<u8>>, Condvar)>,
    writer: Mutex<Box<dyn std::io::Write + Send>>,
    child: Mutex<Box<dyn portable_pty::Child + Send>>,
    reader_thread: Option<std::thread::JoinHandle<()>>,
}

impl TuiSession {
    /// The declared terminal size.
    pub fn size(&self) -> (u16, u16) {
        (self.width, self.height)
    }

    /// The child process's PID, if the PTY backend exposes one.
    /// Used by diagnostics that shell out to `lsof`.
    pub fn pid(&self) -> Option<u32> {
        self.child.lock().ok().and_then(|c| c.process_id())
    }

    /// Run `lsof -i -P` against the child PID and return the output.
    /// Diagnostic only — used to see whether the child opened a
    /// socket to the mock.
    pub fn lsof_network(&self) -> String {
        let Some(pid) = self.pid() else {
            return "(no pid)".to_string();
        };
        let out = std::process::Command::new("lsof")
            .args(["-nP", "-p", &pid.to_string(), "-i"])
            .output();
        match out {
            Ok(o) => String::from_utf8_lossy(&o.stdout).into_owned(),
            Err(e) => format!("(lsof failed: {e})"),
        }
    }

    /// Snapshot of the raw bytes received so far.
    pub fn raw_bytes(&self) -> Vec<u8> {
        self.buf.0.lock().unwrap().clone()
    }

    /// A freshly-parsed `ScreenState` from everything received so far.
    ///
    /// Re-parsing the whole buffer on each call is the simplest way
    /// to avoid the "did I already feed this?" bookkeeping that a
    /// stateful incremental parser would need. A TUI's total output
    /// over a test is a few KB; the parse cost is negligible.
    pub fn screen(&self) -> ScreenState {
        let mut s = ScreenState::new(self.width, self.height);
        s.feed(&self.raw_bytes());
        s
    }

    /// The screen as a string, one line per row, trailing blanks
    /// trimmed. This is what assertions should match against — a
    /// substring search over an unpadded string would accidentally
    /// match across the padding spaces `ScreenState` writes.
    pub fn screen_text(&self) -> String {
        let raw = self.screen().contents();
        raw.lines()
            .map(|l| l.trim_end())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Wait until `pred(&screen_text())` is true, or `timeout` elapses.
    ///
    /// Returns the matching screen text on success; on timeout,
    /// panics with a dump of the actual screen so the failure is
    /// diagnosable without a re-run.
    pub fn wait_for_text(&self, needle: &str, timeout: Duration) -> String {
        let start = Instant::now();
        loop {
            let text = self.screen_text();
            if text.contains(needle) {
                return text;
            }
            if start.elapsed() > timeout {
                let mut msg = String::new();
                msg.push_str(&format!(
                    "timed out after {:?} waiting for {needle:?}\n",
                    timeout
                ));
                msg.push_str("--- raw screen ---\n");
                for (i, line) in text.lines().enumerate() {
                    msg.push_str(&format!("{i:3}|{line}|\n"));
                }
                let raw = self.raw_bytes();
                msg.push_str(&format!("--- {} raw bytes captured ---\n", raw.len()));
                msg.push_str("--- last 2500 raw bytes (escaped) ---\n");
                let start = raw.len().saturating_sub(2500);
                for &b in &raw[start..] {
                    match b {
                        b'\n' => msg.push_str("\\n\n"),
                        b'\r' => msg.push_str("\\r"),
                        b'\t' => msg.push_str("\\t"),
                        0x1b => msg.push_str("\\x1b"),
                        0x07 => msg.push_str("\\a"),
                        b' '..=b'~' => msg.push(b as char),
                        _ => msg.push_str(&format!("\\x{b:02x}")),
                    }
                }
                msg.push_str("\n--- end raw ---\n");
                let _ = &self.stderr_log;
                panic!("{msg}");
            }
            // Wake as soon as bytes arrive; fall back to a short
            // poll so a predicate satisfied by a growing buffer is
            // noticed even if no wake signal fires.
            let (lock, cvar) = &*self.buf;
            let g = lock.lock().unwrap();
            let _ = cvar.wait_timeout(g, Duration::from_millis(100)).unwrap();
        }
    }

    /// Send raw bytes to the TUI (escape sequences, plain text).
    pub fn send_bytes(&self, bytes: &[u8]) {
        let mut w = self.writer.lock().unwrap();
        w.write_all(bytes).expect("write to PTY");
        w.flush().expect("flush PTY");
    }

    /// Send a string as typed characters.
    pub fn send_text(&self, s: &str) {
        self.send_bytes(s.as_bytes());
    }

    /// Send one key event, encoded as the escape sequence a real
    /// terminal would send.
    pub fn send_key(&self, key: KeyCode) {
        self.send_bytes(&encode_key(key, Modifiers::empty()));
    }

    /// Send a key with modifiers.
    pub fn send_key_with(&self, key: KeyCode, mods: Modifiers) {
        self.send_bytes(&encode_key(key, mods));
    }

    /// Kill the child process and join the reader thread.
    pub fn shutdown(&mut self) {
        if let Ok(mut c) = self.child.lock() {
            let _ = c.kill();
        }
        if let Some(t) = self.reader_thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for TuiSession {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Encode a `KeyCode` + `Modifiers` as the bytes a terminal would
/// send for that keypress.
///
/// Covers the keys the tests use: printable chars, Enter, Tab, Esc,
/// Backspace, arrow keys, and Ctrl-letter. Unknown keys encode to
/// nothing rather than a guess — a test that sends an unmapped key
/// should fail loudly, not silently do the wrong thing.
fn encode_key(key: KeyCode, mods: Modifiers) -> Vec<u8> {
    // Ctrl+letter: emit the control byte.
    if mods.contains(Modifiers::CTRL) {
        if let KeyCode::Char(c) = key {
            if c.is_ascii_alphabetic() {
                let byte = (c.to_ascii_lowercase() as u8) - b'a' + 1;
                return vec![byte];
            }
        }
    }

    match key {
        KeyCode::Char(c) => {
            let mut s = String::new();
            s.push(c);
            s.into_bytes()
        }
        KeyCode::Enter => b"\r".to_vec(),
        KeyCode::Tab => b"\t".to_vec(),
        KeyCode::Esc => vec![0x1b],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        _ => Vec::new(),
    }
}

/// Default timeout for `wait_for_text` — generous enough for a cold
/// binary under parallel test load, short enough that a genuine hang
/// is caught within a test-runner's patience.
pub const WAIT: Duration = Duration::from_secs(10);

/// Wait for the TUI to reach its ready state and return the screen.
///
/// Two markers:
/// 1. The model name from the test config — proof the config was read.
/// 2. The startup system message — proof the event loop is dispatching.
pub fn wait_for_ready(session: &TuiSession) -> String {
    session.wait_for_text("mock-model", WAIT);
    session.wait_for_text("Connected", WAIT)
}
