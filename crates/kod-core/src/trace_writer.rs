//! Append-only writer for the `turns.jsonl` file next to the session
//! log (Tier 1.4).
//!
//! One `TraceWriter` per session. Opens `turns.jsonl` in append mode
//! and writes one `TurnTrace` per line. A partial line means the
//! process was killed mid-write; the reader tolerates that by
//! skipping unparseable lines (the same policy the session log
//! uses).

use crate::trace::TurnTrace;
use kod_error::{KodError, Result};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub struct TraceWriter {
    path: PathBuf,
    writer: Mutex<std::fs::File>,
}

impl TraceWriter {
    /// Open `path` for append, creating parents if needed.
    pub fn open(path: PathBuf) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(KodError::Io)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(KodError::Io)?;
        Ok(Self {
            path,
            writer: Mutex::new(file),
        })
    }

    /// Default trace path next to a session log path: same directory,
    /// `turns.jsonl` instead of the session filename.
    pub fn default_for_session(session_path: &Path) -> Option<PathBuf> {
        session_path
            .parent()
            .map(|p| p.join("turns.jsonl"))
    }

    /// Append one trace.
    pub fn record(&self, trace: &TurnTrace) -> Result<()> {
        let line =
            serde_json::to_string(trace).map_err(|e| KodError::Serialization(e.to_string()))?;
        let mut w = self.writer.lock().unwrap();
        writeln!(w, "{}", line).map_err(KodError::Io)?;
        w.flush().map_err(KodError::Io)?;
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Read every trace in `path`, in file order. Unparseable lines are
/// skipped with a warning — a crash mid-write leaves one partial line
/// that is not a real failure.
pub fn read_traces(path: &Path) -> Result<Vec<TurnTrace>> {
    let raw = std::fs::read_to_string(path).map_err(KodError::Io)?;
    let mut out = Vec::new();
    for (i, line) in raw.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<TurnTrace>(line) {
            Ok(t) => out.push(t),
            Err(e) => {
                tracing::warn!(line = i + 1, error = %e, "skipping unparseable turn trace");
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trace::{ToolOutcomeKind, TurnOutcome, TurnTraceBuilder};
    use tempfile::TempDir;

    fn sample(id: u64) -> TurnTrace {
        let mut b = TurnTraceBuilder::new(id, "session");
        b.begin_round("cloud", "claude");
        b.add_usage(100, 20, None, 0.001);
        b.add_tool_call("read_file", "abc".into(), serde_json::json!({}), 5, ToolOutcomeKind::Success, 100, None, None);
        b.finish()
    }

    #[test]
    fn record_then_read_roundtrips() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("turns.jsonl");
        let w = TraceWriter::open(path.clone()).unwrap();
        w.record(&sample(1)).unwrap();
        w.record(&sample(2)).unwrap();
        let traces = read_traces(&path).unwrap();
        assert_eq!(traces.len(), 2);
        assert_eq!(traces[0].id, 1);
        assert_eq!(traces[1].id, 2);
    }

    #[test]
    fn empty_file_reads_empty() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("empty.jsonl");
        std::fs::write(&path, "").unwrap();
        assert!(read_traces(&path).unwrap().is_empty());
    }

    #[test]
    fn a_partial_line_is_skipped_not_fatal() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("partial.jsonl");
        let mut s = serde_json::to_string(&sample(1)).unwrap();
        s.push('\n');
        s.push_str("{\"id\": 2, \"holder\":");
        s.push('\n');
        std::fs::write(&path, s).unwrap();
        let traces = read_traces(&path).unwrap();
        assert_eq!(traces.len(), 1);
        assert_eq!(traces[0].id, 1);
    }

    #[test]
    fn open_creates_parent_dirs() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("a").join("b").join("turns.jsonl");
        let w = TraceWriter::open(path.clone()).unwrap();
        w.record(&sample(1)).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn default_for_session_uses_sibling_file() {
        let p = std::path::PathBuf::from("/home/u/.kod/sessions/2026.jsonl");
        let default = TraceWriter::default_for_session(&p).unwrap();
        assert_eq!(default, std::path::PathBuf::from("/home/u/.kod/sessions/turns.jsonl"));
    }

    #[test]
    fn multiple_writers_append() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("multi.jsonl");
        let a = TraceWriter::open(path.clone()).unwrap();
        a.record(&sample(1)).unwrap();
        let b = TraceWriter::open(path.clone()).unwrap();
        b.record(&sample(2)).unwrap();
        let traces = read_traces(&path).unwrap();
        assert_eq!(traces.len(), 2);
    }

    #[test]
    fn session_writer_share_state_across_clones() {
        // TraceWriter is not Clone (mutex + file); but Arc<TraceWriter>
        // is the intended shape. This test pins the API: a single
        // writer opened once and used from multiple threads appends
        // both records.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("shared.jsonl");
        let w = std::sync::Arc::new(TraceWriter::open(path.clone()).unwrap());
        let w2 = w.clone();
        let h = std::thread::spawn(move || {
            w2.record(&sample(1)).unwrap();
        });
        w.record(&sample(2)).unwrap();
        h.join().unwrap();
        let traces = read_traces(&path).unwrap();
        assert_eq!(traces.len(), 2);
    }

    #[test]
    fn turn_outcome_serializes_cleanly() {
        let mut b = TurnTraceBuilder::new(9, "session");
        b.set_outcome(TurnOutcome::BudgetExhausted, Some("session cap".into()));
        let t = b.finish();
        let s = serde_json::to_string(&t).unwrap();
        assert!(s.contains("\"budget_exhausted\""));
    }
}
