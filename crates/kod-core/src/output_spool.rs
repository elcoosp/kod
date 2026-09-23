//! Output spool for background shell tasks.
//!
//! A background command's output has three consumers with different
//! needs: the model wants a short preview when the job finishes, the
//! user wants the whole thing on disk, and the stall watchdog wants to
//! know *when* output last arrived. One file serves all three: the
//! command appends, the preview reads the tail, and the last-write
//! timestamp is what "stalled" is measured against.
//!
//! The spool is deliberately boring — a file and a couple of counters.
//! A background task that outlives its process leaves its output
//! behind, which is the property that makes the feature useful: a
//! command started before a crash can be inspected after.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How much of the tail a preview shows.
pub const PREVIEW_BYTES: usize = 4 * 1024;

/// One background command's output.
pub struct OutputSpool {
    path: PathBuf,
    /// Bytes written since the spool was created.
    written: u64,
    /// When the last write landed. The stall watchdog measures from
    /// here, not from the job's start — a command that has been
    /// running an hour but wrote a line ten seconds ago is working,
    /// not stalled.
    last_write: Instant,
}

impl OutputSpool {
    /// Create a spool at `path`, truncating any existing file.
    pub fn create(path: impl Into<PathBuf>) -> std::io::Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::File::create(&path)?;
        Ok(Self {
            path,
            written: 0,
            last_write: Instant::now(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Bytes written so far.
    pub fn written(&self) -> u64 {
        self.written
    }

    /// Append a chunk. Bumps the last-write clock — this is what
    /// re-arms a stall.
    pub fn append(&mut self, chunk: &[u8]) -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new().append(true).open(&self.path)?;
        f.write_all(chunk)?;
        self.written += chunk.len() as u64;
        self.last_write = Instant::now();
        Ok(())
    }

    /// How long since the last write.
    pub fn silent_for(&self) -> Duration {
        self.last_write.elapsed()
    }

    /// Whether the spool has been silent longer than `threshold`.
    ///
    /// A zero threshold means "never report a stall" — the caller
    /// turned the watchdog off.
    pub fn is_stalled(&self, threshold: Duration) -> bool {
        !threshold.is_zero() && self.silent_for() >= threshold
    }

    /// The last [`PREVIEW_BYTES`] of output, lossily decoded.
    ///
    /// Reads the tail rather than the whole file: a command that
    /// emitted a gigabyte should cost the preview a 4 KiB read, not a
    /// gigabyte one. The decode is lossy because a background command
    /// may emit anything — the preview is for a human or a model to
    /// scan, not to parse.
    pub fn preview(&self) -> String {
        let Ok(data) = std::fs::read(&self.path) else {
            return String::new();
        };
        let start = data.len().saturating_sub(PREVIEW_BYTES);
        String::from_utf8_lossy(&data[start..]).into_owned()
    }

    /// Mark activity without writing — a heartbeat from a command
    /// that produces no output while it works (`cargo build` before
    /// the first line).
    pub fn touch(&mut self) {
        self.last_write = Instant::now();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_accumulates_and_bumps_the_clock() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut s = OutputSpool::create(tmp.path().join("out.log")).unwrap();
        s.append(b"line one\n").unwrap();
        s.append(b"line two\n").unwrap();
        assert_eq!(s.written(), 18);
        assert!(s.silent_for() < Duration::from_secs(1));
    }

    #[test]
    fn preview_shows_the_tail_not_the_head() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut s = OutputSpool::create(tmp.path().join("out.log")).unwrap();
        // More than PREVIEW_BYTES of 'a', then a distinctive tail.
        s.append(&vec![b'a'; PREVIEW_BYTES + 100]).unwrap();
        s.append(b"THE-END").unwrap();
        let p = s.preview();
        assert!(p.ends_with("THE-END"), "preview must include the tail");
        assert!(p.len() <= PREVIEW_BYTES + 8, "preview is bounded");
    }

    #[test]
    fn is_stalled_respects_the_threshold() {
        let tmp = tempfile::TempDir::new().unwrap();
        let s = OutputSpool::create(tmp.path().join("out.log")).unwrap();
        // Zero threshold disables the watchdog.
        assert!(!s.is_stalled(Duration::ZERO));
        // A large threshold is not yet reached.
        assert!(!s.is_stalled(Duration::from_secs(3600)));
    }

    #[test]
    fn touch_rearms_without_writing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut s = OutputSpool::create(tmp.path().join("out.log")).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let before = s.silent_for();
        s.touch();
        assert!(s.silent_for() < before, "touch must reset the clock");
        assert_eq!(s.written(), 0, "touch writes nothing");
    }

    #[test]
    fn a_missing_file_yields_an_empty_preview() {
        let tmp = tempfile::TempDir::new().unwrap();
        let s = OutputSpool::create(tmp.path().join("out.log")).unwrap();
        std::fs::remove_file(s.path()).unwrap();
        assert_eq!(s.preview(), "");
    }

    #[test]
    fn preview_survives_non_utf8_output() {
        // A background command may emit arbitrary bytes; the preview
        // must not panic on them.
        let tmp = tempfile::TempDir::new().unwrap();
        let mut s = OutputSpool::create(tmp.path().join("out.log")).unwrap();
        s.append(&[0xff, 0xfe, 0x00, 0x41]).unwrap();
        let p = s.preview();
        assert!(p.contains('A'), "the ASCII byte survives: {p:?}");
    }
}
