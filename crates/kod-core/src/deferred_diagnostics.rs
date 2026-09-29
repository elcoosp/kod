//! Delta §7.2: deferred LSP diagnostics queue.
//!
//! # The problem
//!
//! The inline diagnostics pass on a write's own turn waits a bounded
//! time for the language server to publish. A slow rust-analyzer on
//! a large workspace can take seconds; making the model wait all of
//! that is the wrong trade. The write path should stay fast.
//!
//! # The fix
//!
//! A second, background pass keeps watching the same server for a
//! longer budget (`[lsp] deferred_settle_ms`, default 12 s). Anything
//! it sees is pushed into a per-transcript queue. The next turn's
//! prompt block drains the queue and prepends the diagnostics under a
//! `## LSP diagnostics (late)` heading — the model sees the errors on
//! the *next* call, which is where they matter.
//!
//! # Dedup
//!
//! A queue entry carries `(path, line, column, severity, code,
//! message)` — a diagnostic equal to one already queued for the same
//! transcript is dropped on push. The language server publishes its
//! whole list on every change, so without dedup a busy rust-analyzer
//! would queue the same 30 errors N times.
//!
//! # Version guard
//!
//! The background task is spawned with the file's content and the
//! `version` (an opaque monotonic id) it was reading. It only pushes
//! a result if the file's mtime is still what it was at spawn — a
//! write during the wait makes the answer stale and it is dropped.
//! The design's "version guards" in spirit: kod has no LSP document
//! version on the engine side, so mtime stands in.

use std::collections::HashMap;
use std::sync::Mutex;

use kod_tools::check::Diagnostic;

/// The dedup key. `Diagnostic` in `kod-tools` is not `Hash`, so
/// `(file, line, column, severity, code, message)` is spelled out.
fn key(d: &Diagnostic) -> (String, u32, u32, String, Option<String>, String) {
    (
        d.file.clone(),
        d.line,
        d.column,
        d.severity.clone(),
        d.code.clone(),
        d.message.clone(),
    )
}

/// Per-transcript deferred diagnostics.
///
/// Cheap to clone-out and back in because the engine holds it inside
/// the `KodEngine` behind a lock; the methods take `&self` so no
/// `RwLock` around the map is needed here.
#[derive(Debug, Default)]
pub struct DeferredDiagnostics {
    /// `holder` → list of diagnostics waiting to be drained into the
    /// next prompt block for that transcript.
    by_holder: Mutex<HashMap<String, Vec<Diagnostic>>>,
}

impl DeferredDiagnostics {
    pub fn new() -> Self {
        Self::default()
    }

    /// Push diagnostics for `holder`. A diagnostic equal to one
    /// already queued is dropped. Returns the number of *new* entries
    /// (0 when everything was a duplicate, or the input was empty).
    pub fn push(&self, holder: &str, diags: &[Diagnostic]) -> usize {
        if diags.is_empty() {
            return 0;
        }
        let mut g = self.by_holder.lock().expect("deferred_diagnostics poisoned");
        let entry = g.entry(holder.to_string()).or_default();
        let existing: std::collections::HashSet<_> = entry.iter().map(key).collect();
        let before = entry.len();
        for d in diags {
            if !existing.contains(&key(d)) {
                entry.push(d.clone());
            }
        }
        entry.len() - before
    }

    /// Take every queued diagnostic for `holder`, emptying that
    /// transcript's queue. Called by the prompt builder at the start
    /// of a turn.
    pub fn take(&self, holder: &str) -> Vec<Diagnostic> {
        let mut g = self.by_holder.lock().expect("deferred_diagnostics poisoned");
        g.remove(holder).unwrap_or_default()
    }

    /// Number of queued diagnostics for `holder`.
    pub fn len_for(&self, holder: &str) -> usize {
        self.by_holder
            .lock()
            .expect("deferred_diagnostics poisoned")
            .get(holder)
            .map(|v| v.len())
            .unwrap_or(0)
    }

    /// True when nothing is queued for any transcript.
    pub fn is_empty(&self) -> bool {
        self.by_holder
            .lock()
            .expect("deferred_diagnostics poisoned")
            .values()
            .all(|v| v.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(file: &str, line: u32, msg: &str) -> Diagnostic {
        Diagnostic {
            file: file.to_string(),
            line,
            column: 1,
            severity: "error".to_string(),
            code: None,
            message: msg.to_string(),
        }
    }

    #[test]
    fn empty_push_returns_zero_and_queues_nothing() {
        let q = DeferredDiagnostics::new();
        assert_eq!(q.push("h", &[]), 0);
        assert!(q.take("h").is_empty());
    }

    #[test]
    fn push_then_take_round_trips() {
        let q = DeferredDiagnostics::new();
        let n = q.push("h", &[d("a.rs", 3, "boom"), d("b.rs", 1, "nope")]);
        assert_eq!(n, 2);
        let out = q.take("h");
        assert_eq!(out.len(), 2);
        // Take drains.
        assert!(q.take("h").is_empty());
    }

    #[test]
    fn duplicates_are_dropped_on_push() {
        let q = DeferredDiagnostics::new();
        let one = d("a.rs", 3, "boom");
        assert_eq!(q.push("h", std::slice::from_ref(&one)), 1);
        // Push the same one again plus a new one — only the new one
        // is a fresh entry.
        let two = d("b.rs", 7, "other");
        assert_eq!(q.push("h", &[one.clone(), two]), 1);
        assert_eq!(q.len_for("h"), 2);
    }

    #[test]
    fn holders_are_isolated() {
        let q = DeferredDiagnostics::new();
        q.push("a", &[d("x.rs", 1, "a-err")]);
        q.push("b", &[d("x.rs", 1, "b-err")]);
        assert_eq!(q.len_for("a"), 1);
        assert_eq!(q.len_for("b"), 1);
        let a = q.take("a");
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].message, "a-err");
        // b is untouched.
        assert_eq!(q.len_for("b"), 1);
    }

    #[test]
    fn is_empty_ignores_unknown_holders() {
        let q = DeferredDiagnostics::new();
        assert!(q.is_empty());
        q.push("a", &[d("x.rs", 1, "err")]);
        assert!(!q.is_empty());
        q.take("a");
        assert!(q.is_empty());
    }
}
