//! Documented cache invalidations.
//!
//! A KV-cache miss is invisible without a journal: the provider reports
//! `cache_read = 0`, the client has no idea whether that was a
//! legitimate prefix break (compaction, model switch, a new MCP tool)
//! or a harness bug. This module records the legitimate causes so the
//! two are distinguishable — jcode's rule (design §4.3): "an empty
//! journal around a harness-caused miss is itself signal."
//!
//! The journal is append-only JSONL, redacted, and bounded. It is
//! never read on the hot path — [`record`] is fire-and-forget and
//! drops on error.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::OnceLock;

/// Why the cacheable prefix changed.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InvalidationCause {
    /// The tool definitions array differs from the previous request.
    /// Expected on a late MCP registration; unexpected otherwise.
    ToolSurfaceChanged { previous_fingerprint: u64, current_fingerprint: u64, reason: String },
    /// `compact_history_for` removed messages.
    Compaction { removed: usize },
    /// The current model reference changed.
    ModelSwitch { from: String, to: String },
    /// The cacheable system segment changed (identity, repo map, or
    /// the AGENTS.md snapshot if one is present).
    SystemPromptChanged { fingerprint: u64 },
}

impl InvalidationCause {
    fn kind(&self) -> &'static str {
        match self {
            Self::ToolSurfaceChanged { .. } => "tool_surface_changed",
            Self::Compaction { .. } => "compaction",
            Self::ModelSwitch { .. } => "model_switch",
            Self::SystemPromptChanged { .. } => "system_prompt_changed",
        }
    }
}

fn journal_path() -> Option<PathBuf> {
    let dir = dirs::home_dir()?.join(".kod");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join("cache_journal.jsonl"))
}

struct JournalWriter {
    file: Mutex<std::fs::File>,
}

impl JournalWriter {
    fn open() -> Option<Self> {
        let path = journal_path()?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok()?;
        Some(Self { file: Mutex::new(file) })
    }
}

fn writer() -> Option<&'static JournalWriter> {
    static WRITER: OnceLock<Option<JournalWriter>> = OnceLock::new();
    WRITER.get_or_init(JournalWriter::open).as_ref()
}

/// Record a legitimate invalidation. Best-effort: a failed write never
/// affects the caller. The `ts_ms` field is wall-clock at write.
pub fn record(cause: InvalidationCause) {
    let Some(w) = writer() else { return };
    let Ok(mut file) = w.file.lock() else { return };
    let ts_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let line = serde_json::json!({
        "ts_ms": ts_ms,
        "kind": cause.kind(),
        "cause": cause,
    });
    let _ = writeln!(file, "{line}");
}

/// Read the last `n` entries for `/debug cache`. Returns an empty vec
/// on any error so a caller can print "no recorded invalidations"
/// rather than failing the debug view.
pub fn recent(n: usize) -> Vec<serde_json::Value> {
    let Some(path) = journal_path() else { return Vec::new() };
    let Ok(raw) = std::fs::read_to_string(&path) else { return Vec::new() };
    let mut out: Vec<serde_json::Value> = raw
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .collect();
    if out.len() > n {
        out.drain(..out.len() - n);
    }
    out
}
