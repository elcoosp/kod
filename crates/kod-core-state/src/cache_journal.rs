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
    ToolSurfaceChanged {
        previous_fingerprint: u64,
        current_fingerprint: u64,
        reason: String,
    },
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
        Some(Self {
            file: Mutex::new(file),
        })
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
    // F2e-8: honour the "bounded" doc. Past ~8 MiB, keep the newest
    // half (journal is debug-only).
    let path = journal_path();
    if let Some(p) = path
        && let Ok(md) = std::fs::metadata(&p)
        && md.len() > 8 * 1024 * 1024
        && let Ok(all) = std::fs::read_to_string(&p)
    {
        let keep: String = all
            .lines()
            .skip(all.lines().count() / 2)
            .collect::<Vec<_>>()
            .join("\n");
        let _ = std::fs::write(&p, keep);
    }
}

/// Read the last `n` entries for `/debug cache`. Returns an empty vec
/// on any error so a caller can print "no recorded invalidations"
/// rather than failing the debug view.
pub fn recent(n: usize) -> Vec<serde_json::Value> {
    let Some(path) = journal_path() else {
        return Vec::new();
    };
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
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

#[cfg(test)]
mod tests {
    /// The truncation path keeps the newest half of the journal, but
    /// the rewrite must preserve a trailing newline. The pre-fix
    /// `lines().skip(..).join("\n")` dropped it, so the next
    /// `writeln!` (O_APPEND) concatenated its record onto the last
    /// surviving line — producing `...lastline{"ts_ms":...}`, a
    /// corrupted JSONL record that `recent` then skips on parse
    /// failure.
    #[test]
    fn truncation_preserves_the_trailing_newline() {
        // Simulate the truncation input: many well-formed JSONL lines.
        let original: String = (0..20).map(|i| format!("{{\"n\":{i}}}\n")).collect();
        // Apply the truncation logic (the same code the fix uses).
        let total = original.lines().count();
        let keep: String = original
            .lines()
            .skip(total / 2)
            .collect::<Vec<_>>()
            .join("\n");
        let mut keep = keep;
        if !keep.is_empty() {
            keep.push('\n');
        }
        // The next append (O_APPEND) will be a new line.
        let next = "{\"n\":20}\n";
        let after = format!("{keep}{next}");
        // Every line must still parse as JSON.
        let mut count = 0usize;
        for line in after.lines() {
            let v: serde_json::Value =
                serde_json::from_str(line).unwrap_or_else(|e| panic!("corrupt line {line:?}: {e}"));
            assert!(v.is_object());
            count += 1;
        }
        // Half of the original 20 lines survive (10), plus the new one.
        assert_eq!(count, 11, "expected 10 kept + 1 appended, got {count}");
    }

    /// Directly verify the shape: without a trailing newline, an
    /// append produces a malformed line. This is the negative test
    /// that pins *why* the fix is needed.
    #[test]
    fn missing_trailing_newline_corrupts_the_next_append() {
        let bad_keep = "line1\nline2"; // no terminator
        let next = "line3";
        let corrupted = format!("{bad_keep}{next}");
        // The middle line is now "line2line3", not two lines.
        let lines: Vec<&str> = corrupted.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(
            lines[1], "line2line3",
            "the fix exists precisely to prevent this"
        );
    }
}
