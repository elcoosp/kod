//! Presence markers: which sessions are alive right now.
//!
//! A session list needs to answer "which of these is still running"
//! without attaching to any of them and without scanning a directory
//! of transcripts. A one-file-per-session marker directory does that:
//! the file's existence is the presence, its contents are the owning
//! pid, and liveness is a `kill(pid, 0)` — the cheapest liveness check
//! a Unix gives you.
//!
//! A marker whose pid is gone is a crashed session. That is the useful
//! signal: the transcript is on disk, so a crashed session can be
//! recovered rather than lost.
//!
//! Markers are removed on a clean exit. A `SIGKILL` leaves one
//! behind, which is exactly the case the liveness check exists to
//! catch.

use std::path::{Path, PathBuf};

/// Which kind of session a marker describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkerKind {
    /// An interactive session — the TUI or a `chat` run.
    Interactive,
    /// A session currently generating. Present only while a turn is
    /// in flight, so a peek can tell "idle" from "working".
    Streaming,
    /// A swarm agent or other internal session, hidden from the
    /// user-facing list.
    Internal,
}

impl MarkerKind {
    fn dir_name(self) -> &'static str {
        match self {
            Self::Interactive => "active_pids",
            Self::Streaming => "streaming_pids",
            Self::Internal => "internal_pids",
        }
    }
}

/// The marker directory for `kind`, under `root`.
pub fn marker_dir(root: &Path, kind: MarkerKind) -> PathBuf {
    root.join(kind.dir_name())
}

/// Write a marker for `session` owned by the current process.
///
/// Best-effort: a failure means the session is simply absent from the
/// list, which is better than failing the session's own startup.
pub fn mark(root: &Path, kind: MarkerKind, session: &str) -> Option<PathBuf> {
    let dir = marker_dir(root, kind);
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(sanitize(session));
    std::fs::write(&path, std::process::id().to_string()).ok()?;
    Some(path)
}

/// Remove a session's marker.
pub fn unmark(root: &Path, kind: MarkerKind, session: &str) {
    let _ = std::fs::remove_file(marker_dir(root, kind).join(sanitize(session)));
}

/// One live session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveSession {
    pub name: String,
    pub pid: u32,
}

/// Every marker of `kind` whose process is still alive, sorted by
/// name.
///
/// A marker whose pid is gone is left on disk — removing it here would
/// hide the crash from a caller that wants to offer recovery.
pub fn list(root: &Path, kind: MarkerKind) -> Vec<LiveSession> {
    let dir = marker_dir(root, kind);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out: Vec<LiveSession> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let pid: u32 = std::fs::read_to_string(e.path()).ok()?.trim().parse().ok()?;
            process_alive(pid).then_some(LiveSession { name, pid })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Whether `pid` names a live process.
///
/// `kill(pid, 0)` performs the permission and existence checks without
/// sending a signal. `ESRCH` means no such process; `EPERM` means one
/// exists but is not ours — which is still alive, and the honest
/// answer is `true`.
#[cfg(unix)]
pub fn process_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    // SAFETY: `kill` with signal 0 has no effect beyond the check; it
    // is the documented, portable way to test for a process.
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if rc == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
pub fn process_alive(_pid: u32) -> bool {
    // No portable check without libc; report "alive" so a caller does
    // not offer to recover a session that is running.
    true
}

/// A session name safe to use as a file name.
fn sanitize(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    if out.is_empty() {
        out.push_str("session");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_replaces_path_separators() {
        assert_eq!(sanitize("swarm/agent-1"), "swarm_agent-1");
        assert_eq!(sanitize("a b c"), "a_b_c");
        assert_eq!(sanitize(""), "session");
    }

    #[test]
    fn the_current_process_is_alive() {
        assert!(process_alive(std::process::id()));
    }

    #[test]
    fn pid_zero_is_not_alive() {
        // Not a process; the guard exists so a marker written as "0"
        // never reads as live.
        assert!(!process_alive(0));
    }

    #[test]
    fn a_mark_round_trips_through_list() {
        let tmp = tempfile::TempDir::new().unwrap();
        mark(tmp.path(), MarkerKind::Interactive, "session-a");
        let live = list(tmp.path(), MarkerKind::Interactive);
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].name, "session-a");
        assert_eq!(live[0].pid, std::process::id());
    }

    #[test]
    fn unmark_removes_the_entry() {
        let tmp = tempfile::TempDir::new().unwrap();
        mark(tmp.path(), MarkerKind::Streaming, "s");
        unmark(tmp.path(), MarkerKind::Streaming, "s");
        assert!(list(tmp.path(), MarkerKind::Streaming).is_empty());
    }

    #[test]
    fn a_dead_pid_is_not_listed_but_its_marker_survives() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = marker_dir(tmp.path(), MarkerKind::Interactive);
        std::fs::create_dir_all(&dir).unwrap();
        // Pid 1 exists on Unix; use a pid that cannot (above the
        // usual maximum) so the check is meaningful.
        std::fs::write(dir.join("crashed"), "4294967290").unwrap();
        assert!(list(tmp.path(), MarkerKind::Interactive).is_empty());
        assert!(dir.join("crashed").exists(), "the crash is left visible");
    }

    #[test]
    fn listing_a_missing_directory_is_empty() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert!(list(tmp.path(), MarkerKind::Internal).is_empty());
    }
}
