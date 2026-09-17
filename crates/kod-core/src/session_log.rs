//! Session log: a JSONL record of every tool call the engine makes.
//!
//! Two uses, both worth the file:
//!
//! 1. **Debugging.** "What actually happened in that run?" is a
//!    question the terminal transcript cannot answer once the tool
//!    rows have scrolled past. A JSONL file can be grepped, diffed,
//!    and — most usefully — replayed.
//! 2. **Regression suite.** `kod replay <log>` re-executes the tool
//!    calls without the model. The same tool sequence against a new
//!    commit of the codebase is a concrete check that the tools still
//!    work; a diff of the results is a concrete check that the
//!    codebase still behaves the same way.
//!
//! The log is append-only and opened with `O_APPEND`; concurrent
//! writers append without an advisory lock. A single line is small
//! enough that the kernel's atomic-append guarantee on a regular file
//! is sufficient — the same reasoning `persist_history_entry` uses in
//! the TUI.

use kod_error::{KodError, Result};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// One recorded event. Tagged by `kind` so a future variant can be
/// added without a schema migration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionEntry {
    /// A tool call and its result.
    ToolCall { id: None,
        /// Unix milliseconds when the call started.
        timestamp_ms: u64,
        /// The engine-transcript key the call ran under (`session` for
        /// the interactive session, `swarm:<agent-id>` for a swarm
        /// agent).
        holder: String,
        tool_name: String,
        arguments: serde_json::Value,
        /// Wall time the call took, in milliseconds.
        duration_ms: u64,
        /// The result, in the same shape the model saw. Success carries
        /// the raw `Success(Value)` payload under `success`, an
        /// `Error(String)` under `error`, and a
        /// `RequiresConfirmation` under `requires_confirmation`.
        result: serde_json::Value,
    },
    /// The routing layer gave up on one endpoint and moved to the next
    /// in the chain (AD-15). Written once per transition, so a session
    /// log carries a clean audit trail of which endpoint served which
    /// turn.
    ModelFallback {
        timestamp_ms: u64,
        /// The transcript key the fallback happened on ("session",
        /// "swarm:agent-3", ...).
        holder: String,
        from: String,
        to: String,
        error: String,
    },
    /// Cost of one provider call, in USD. Written after every
    /// completion that reported usage and an endpoint with
    /// `pricing` configured. Endpoints without pricing log nothing
    /// here — the field is not a guess.
    Cost {
        timestamp_ms: u64,
        holder: String,
        endpoint: String,
        model: String,
        prompt_tokens: usize,
        completion_tokens: usize,
        cost_usd: f64,
    },
    /// A policy decision made before a tool call (D3, AD-15).
    /// One entry per tool call, so the JSONL carries an audit trail
    /// of every allow/deny/ask the engine answered.
    PolicyDecision {
        timestamp_ms: u64,
        holder: String,
        tool_name: String,
        /// "allow" | "deny" | "ask"
        outcome: String,
        /// Human-readable description of the rule that fired.
        rule: String,
        /// Which layer produced the decision
        /// ("preset" | "global-config" | "project-policy" |
        ///  "cli-override" | "session-deny").
        source: String,
    },
}

/// Append-only writer for a session log.
pub struct SessionRecorder {
    path: PathBuf,
    writer: Mutex<std::fs::File>,
}

impl SessionRecorder {
    /// Open `path` for append, creating it and any missing parent
    /// directories. An existing file is appended to, never truncated —
    /// a `kod replay` run can then be handed a file that has been
    /// extended across multiple sessions.
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

    /// Append one entry. Flushes after every write: a crash mid-session
    /// should leave a complete log up to the last call, not a buffered
    /// prefix that `kod replay` cannot parse.
    pub fn record(&self, entry: &SessionEntry) -> Result<()> {
        let line = serde_json::to_string(entry)
            .map_err(|e| KodError::Serialization(e.to_string()))?;
        let mut w = self.writer.lock().unwrap();
        writeln!(w, "{}", line).map_err(KodError::Io)?;
        w.flush().map_err(KodError::Io)?;
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Flush any buffered bytes. `record()` already flushes per line, so
    /// this is a belt-and-braces call from `shutdown()`: it is a no-op
    /// when there is nothing pending, and it makes the shutdown contract
    /// explicit ("everything written by this recorder is on disk when
    /// this returns") without relying on the per-line flush not
    /// regressing later.
    pub fn flush(&self) -> Result<()> {
        let mut w = self.writer.lock().unwrap();
        w.flush().map_err(KodError::Io)?;
        Ok(())
    }
}

/// Read every entry in `path`, in file order. A malformed line is a
/// hard error — the file is machine-written, and a partial line means
/// the log is corrupt, not that the reader should skip.
pub fn read_session(path: &Path) -> Result<Vec<SessionEntry>> {
    let raw = std::fs::read_to_string(path).map_err(KodError::Io)?;
    let mut out = Vec::new();
    for (i, line) in raw.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<SessionEntry>(line) {
            Ok(e) => out.push(e),
            Err(e) => {
                return Err(KodError::Deserialization(format!(
                    "line {}: {}",
                    i + 1,
                    e
                )));
            }
        }
    }
    Ok(out)
}

/// Default session-log path for a fresh session:
/// `~/.kod/sessions/<unix-ms>.jsonl`. A caller that wants a stable
/// path (a test, a script that names its own log) sets
/// `KOD_SESSION_LOG` and this is not consulted.
pub fn default_session_path() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    Some(
        home.join(".kod")
            .join("sessions")
            .join(format!("{ts}.jsonl")),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn sample_entry() -> SessionEntry {
        SessionEntry::ToolCall { id: None,
            timestamp_ms: 1_700_000_000_000,
            holder: "session".to_string(),
            tool_name: "read_file".to_string(),
            arguments: serde_json::json!({"path": "src/main.rs"}),
            duration_ms: 7,
            result: serde_json::json!({"success": {"path": "src/main.rs", "content": "fn main() {}"}}),
        }
    }

    #[test]
    fn record_then_read_roundtrips() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("session.jsonl");
        let recorder = SessionRecorder::open(path.clone()).unwrap();
        recorder.record(&sample_entry()).unwrap();
        recorder.record(&sample_entry()).unwrap();

        let entries = read_session(&path).unwrap();
        assert_eq!(entries.len(), 2);
        match &entries[0] {
            SessionEntry::ToolCall { id: None,
                tool_name, holder, ..
            } => {
                assert_eq!(tool_name, "read_file");
                assert_eq!(holder, "session");
            }
            other => panic!("unexpected entry kind: {other:?}"),
        }
    }

    #[test]
    fn empty_file_reads_empty() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("empty.jsonl");
        std::fs::write(&path, "").unwrap();
        assert!(read_session(&path).unwrap().is_empty());
    }

    #[test]
    fn malformed_line_is_an_error_naming_the_line() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("bad.jsonl");
        std::fs::write(&path, "{\"kind\":\"tool_call\"\n").unwrap();
        let err = read_session(&path).unwrap_err();
        assert!(err.to_string().contains("line 1"), "got: {err}");
    }

    #[test]
    fn open_creates_parent_directories() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("a").join("b").join("session.jsonl");
        let recorder = SessionRecorder::open(path.clone()).unwrap();
        recorder.record(&sample_entry()).unwrap();
        assert!(path.exists());
    }
}
