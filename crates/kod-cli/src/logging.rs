//! Session-safe tracing plumbing.
//!
//! The subscriber must stay reachable during config load (H-D9: a
//! config-parse warning that lands in the void leaves the user on
//! silent defaults), but once the TUI owns the terminal — alternate
//! screen + raw mode — every raw stderr write garbles the ratatui
//! frame: the line lands at the live cursor, interleaves with the
//! frame diff, and pushes transcript rows over the input box. The
//! diff-based redraw never repairs cells it believes are unchanged,
//! so the damage persists.
//!
//! [`SessionSafeWriter`] therefore routes output by terminal
//! ownership, tracked in [`kod_types::term`]:
//!
//! - TUI inactive (CLI subcommands, pre-TUI config load): stderr, as
//!   before.
//! - TUI active: `~/.kod/session.log` (append mode). If the file
//!   cannot be opened the events are dropped — losing a log line is
//!   strictly better than garbling the user's screen.

use kod_types::term;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

/// Writes tracing events to stderr until the TUI takes over the
/// terminal, then to `~/.kod/session.log`.
///
/// Install with `.with_writer(SessionSafeWriter::default())`. The
/// `KOD_LOG` env var overrides the file location; tests can inject a
/// path directly via [`SessionSafeWriter::with_log_path`].
pub struct SessionSafeWriter {
    file: Mutex<Option<File>>,
    /// Explicit log path (tests). `None` resolves at first use from
    /// `KOD_LOG`, falling back to `~/.kod/session.log`.
    path: Option<PathBuf>,
}

/// Borrowing handle returned by [`tracing_subscriber::fmt::MakeWriter`].
pub struct SessionSafeWriterHandle<'a> {
    inner: &'a SessionSafeWriter,
}

impl SessionSafeWriter {
    /// Resolve the log path: injected path, then `KOD_LOG`, then
    /// `~/.kod/session.log`. `None` if no home directory is known.
    fn log_path(&self) -> Option<PathBuf> {
        if let Some(p) = &self.path {
            return Some(p.clone());
        }
        if let Ok(p) = std::env::var("KOD_LOG") {
            return Some(PathBuf::from(p));
        }
        dirs::home_dir().map(|h| h.join(".kod").join("session.log"))
    }

    /// Pin the log file location (tests, CI). Takes precedence over
    /// `KOD_LOG` and the default home path.
    pub fn with_log_path(path: PathBuf) -> Self {
        Self {
            file: Mutex::new(None),
            path: Some(path),
        }
    }

    fn write_shared(&self, buf: &[u8]) -> std::io::Result<usize> {
        if !term::tui_active() {
            return std::io::stderr().write_all(buf).map(|_| buf.len());
        }
        let Ok(mut slot) = self.file.lock() else {
            return Ok(buf.len());
        };
        if slot.is_none() {
            *slot = self.log_path().and_then(|path| {
                if let Some(parent) = path.parent() {
                    // Best-effort: a missing .kod directory is not fatal.
                    let _ = std::fs::create_dir_all(parent);
                }
                {
                    // T4-H9: cap session.log at 50 MiB. Without rotation, a
                    // debug-level session can grow unbounded.
                    const MAX_LOG_BYTES: u64 = 50 * 1024 * 1024;
                    if let Ok(md) = std::fs::metadata(&path)
                        && md.len() > MAX_LOG_BYTES
                    {
                        let _ = std::fs::remove_file(&path);
                    }
                    // Bug-hunt: the session log carries the same
                    // sensitive payload as the session/trace files
                    // (`RUST_LOG=debug` includes prompt and tool
                    // framing). Default 0644 is world-readable on a
                    // shared host; make the new file owner-only.
                    {
                        let mut opts = OpenOptions::new();
                        opts.create(true).append(true);
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::OpenOptionsExt;
                            opts.mode(0o600);
                        }
                        opts.open(path).ok()
                    }
                }
            });
        }
        match slot.as_mut() {
            Some(file) => file.write_all(buf).map(|_| buf.len()),
            // Log unavailable: drop the line rather than garble the frame.
            None => Ok(buf.len()),
        }
    }

    fn flush_shared(&self) -> std::io::Result<()> {
        if !term::tui_active() {
            return std::io::stderr().flush();
        }
        if let Ok(mut slot) = self.file.lock() {
            if let Some(file) = slot.as_mut() {
                return file.flush();
            }
        }
        Ok(())
    }
}

impl Default for SessionSafeWriter {
    fn default() -> Self {
        Self {
            file: Mutex::new(None),
            path: None,
        }
    }
}

impl<'a> Write for SessionSafeWriterHandle<'a> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.inner.write_shared(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush_shared()
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SessionSafeWriter {
    type Writer = SessionSafeWriterHandle<'a>;

    fn make_writer(&'a self) -> Self::Writer {
        SessionSafeWriterHandle { inner: self }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sequential on purpose: the terminal-ownership flag is global,
    /// so parallel tests would race on it.
    #[test]
    fn routes_by_terminal_ownership() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("session.log");
        let writer = SessionSafeWriter::with_log_path(log.clone());

        // TUI inactive → stderr; nothing touches the log file.
        term::set_tui_active(false);
        writer
            .write_shared(b"pre-tui\n")
            .expect("stderr write succeeds");
        assert!(!log.exists(), "file must not be created pre-TUI");

        // TUI active → the log file, never the terminal.
        term::set_tui_active(true);
        writer
            .write_shared(b"hello from a garble-free session\n")
            .expect("file write succeeds");
        writer.flush_shared().expect("flush");
        term::set_tui_active(false);
        let contents = std::fs::read_to_string(&log).expect("log contents");
        assert!(contents.contains("garble-free"));

        // Unavailable log (a directory is not openable for append):
        // drop silently instead of failing or garbling.
        term::set_tui_active(true);
        let bad = SessionSafeWriter::with_log_path(dir.path().to_path_buf());
        let n = bad.write_shared(b"dropped\n").expect("no error");
        assert_eq!(n, b"dropped\n".len());
        term::set_tui_active(false);
    }
}
