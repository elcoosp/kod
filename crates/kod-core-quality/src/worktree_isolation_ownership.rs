//! Isolation ownership markers (borrow from oh-my-pi, delta §11.11).
//!
//! # The problem
//!
//! A swarm run creates one worktree per agent under
//! `<repo>/.kod/worktrees/<slug>`. If the process that created them
//! crashes — SIGKILL, a panicked embedding host, a docker stop —
//! `Drop for WorktreeManager` never runs, and the worktrees stay on
//! disk with their branches. The next run, finding the slugs taken,
//! refuses to re-create them.
//!
//! # The marker
//!
//! Each worktree gets a `.kod-isolation-owner.json` file at its root
//! with `{ pid, id, start_token }`. The `start_token` is a
//! process-start fingerprint: on Linux it is `/proc/<pid>/stat` field
//! 22 (the process's start tick since boot); on other platforms it is
//! a wall-clock stamp recorded when the marker is written. The token
//! is what distinguishes "pid 1234 is still the process that wrote
//! this marker" from "pid 1234 was recycled and now belongs to
//! something else."
//!
//! # The reaper
//!
//! [`reap_dead`] walks a repo's `.kod/worktrees/` directory, reads
//! each marker, and treats a worktree as reapable when:
//!
//! * the pid is gone (`ESRCH`), **or**
//! * the pid exists but its current start token does not match the
//!   marker's.
//!
//! A worktree with **no marker** is left alone: a hand-created
//! worktree, or one created by a kod version older than this module,
//! is not the reaper's business. A worktree whose marker names the
//! *current* process is also left alone (a concurrent manager in the
//! same process).
//!
//! # What this is NOT
//!
//! * Not the worktree remover. `reap_dead` reports the paths it
//!   considers stale; actually calling `git worktree remove` on each
//!   is the caller's step, because removing a worktree is a
//!   repository mutation the caller may want to gate.
//! * Not a sandbox escape. A marker that says a process is alive is
//!   honoured even if the process is unrelated; the start-token check
//!   is what makes that case rare.

use std::path::{Path, PathBuf};

use kod_error::{KodError, Result};

/// The marker filename. Lives at the root of the worktree.
pub const MARKER_FILENAME: &str = ".kod-isolation-owner.json";

/// The on-disk shape. Kept minimal so a hand-inspection is
/// instantaneous.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct IsolationOwnership {
    /// The pid of the process that created the worktree.
    pub pid: u32,
    /// Free-form identifier (a swarm run id, a branch name). Not
    /// interpreted; used for logging.
    pub id: String,
    /// A process-start fingerprint. See the module docs.
    pub start_token: String,
}

impl IsolationOwnership {
    /// Build a marker for `id` on the current process.
    pub fn for_current_process(id: impl Into<String>) -> Self {
        Self {
            pid: std::process::id(),
            id: id.into(),
            start_token: current_start_token(),
        }
    }

    /// Whether the marker names a process that is currently alive
    /// with the same start token.
    ///
    /// `true` when the pid is the current process (trivially alive).
    /// `false` when the pid is gone. `false` when the pid exists but
    /// its start token differs (a recycled pid). On a platform where
    /// the start token cannot be read, the function degrades to "is
    /// the pid alive" — documented, and the case is rare enough that
    /// a false positive leaves one worktree to reap next run.
    pub fn is_alive(&self) -> bool {
        is_pid_alive(self.pid) && token_matches(self.pid, &self.start_token)
    }
}

/// Write the marker for `id` at the worktree root.
pub fn write_marker(worktree: &Path, id: impl Into<String>) -> Result<()> {
    let marker = IsolationOwnership::for_current_process(id);
    let body =
        serde_json::to_vec_pretty(&marker).map_err(|e| KodError::Serialization(e.to_string()))?;
    let path = worktree.join(MARKER_FILENAME);
    std::fs::write(&path, body).map_err(KodError::Io)?;
    Ok(())
}

/// Read the marker at the worktree root, if any.
pub fn read_marker(worktree: &Path) -> Option<IsolationOwnership> {
    let path = worktree.join(MARKER_FILENAME);
    let body = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&body).ok()
}

/// Remove the marker from a worktree, if present. Called by a clean
/// shutdown so a worktree that is about to be removed does not look
/// stale to a concurrent reaper.
pub fn remove_marker(worktree: &Path) -> Result<()> {
    let path = worktree.join(MARKER_FILENAME);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(KodError::Io(e)),
    }
}

/// Walk `<repo>/.kod/worktrees/` and report every worktree whose
/// marker names a process that is provably dead.
///
/// A worktree without a marker, or whose marker names a live process,
/// is skipped. The caller is responsible for removing each reported
/// path (a `git worktree remove --force` is the intended step; this
/// function does not mutate the repo).
pub fn reap_dead(repo: &Path) -> Result<Vec<PathBuf>> {
    let worktrees_dir = repo.join(".kod").join("worktrees");
    let entries = match std::fs::read_dir(&worktrees_dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(KodError::Io(e)),
    };
    let mut dead = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(marker) = read_marker(&path) else {
            continue;
        };
        if marker.is_alive() {
            continue;
        }
        dead.push(path);
    }
    Ok(dead)
}

/// A process-start fingerprint for `pid`.
///
/// On Linux, `/proc/<pid>/stat` field 22 is the process's start time
/// in clock ticks since boot — the canonical "is this the same
/// process" answer. On other platforms, the fallback is the
/// current-time-in-millis at write time, which detects pid reuse
/// only approximately. Documented as a known limitation; the caller
/// that cares about exactness is Linux in practice.
pub fn process_start_token(pid: u32) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // Field 22 is the starttime. The stat format is:
        //   pid (comm) state ppid pgrp session tty_nr tpgid flags
        //   minflt cminflt majflt cmajflt utime stime cutime cstime
        //   priority nice num_threads itrealvalue starttime ...
        // `comm` may contain spaces/parens, so split after the last `)`.
        let after_comm = stat.rsplit_once(')').map(|(_, rest)| rest)?;
        let fields: Vec<&str> = after_comm.split_whitespace().collect();
        // After the `)` the fields are state (0) ppid (1) ... starttime
        // is field 22 in the kernel's 1-based numbering; after stripping
        // pid and comm we are at index 0 = state, so starttime is at
        // index 22 - 3 = 19.
        fields.get(19).map(|s| s.to_string())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
    }
}

/// A start token for the current process. If the platform cannot
/// provide one, use a wall-clock stamp so a marker still differs
/// between two runs.
fn current_start_token() -> String {
    if let Some(t) = process_start_token(std::process::id()) {
        return t;
    }
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("wall:{now_ms}")
}

/// Whether a pid is alive in the "does a process with this pid exist"
/// sense.
///
/// Uses `kill(pid, 0)`: a `0` signal is a permission/existence probe
/// that never delivers. `EPERM` means the pid exists but belongs to a
/// user we cannot signal — treat that as alive. `ESRCH` means gone.
#[cfg(unix)]
fn is_pid_alive(pid: u32) -> bool {
    use std::process::Command;
    // `kill -0 <pid>` via the shell is portable across unix and lets
    // us avoid a libc dependency; the cost is one spawn per marker,
    // and a run has at most a few dozen.
    let Ok(status) = Command::new("kill").arg("-0").arg(pid.to_string()).status() else {
        // `kill` not on PATH: degrade to "assume alive" so we never
        // reap a worktree whose process might still be running.
        return true;
    };
    status.success()
}

#[cfg(not(unix))]
fn is_pid_alive(_pid: u32) -> bool {
    // No portable probe on Windows without a native call; assume
    // alive and document the gap. A stale worktree on Windows needs
    // an explicit `kod worktree gc`.
    true
}

/// Whether the current start token for `pid` matches `token`. `false`
/// when the token cannot be read (the process is likely gone or the
/// platform does not expose it).
fn token_matches(pid: u32, token: &str) -> bool {
    match process_start_token(pid) {
        Some(current) => current == token,
        // No token available: a Linux box with a very old kernel, or
        // a non-Linux platform with a wall-clock fallback. Treat the
        // token as matching — the live-pid check is the only signal
        // we have and it already said the pid exists.
        None => true,
    }
}

/// Monotonic process-local counter used to break ties between
/// concurrent atomic-replace writers in the same process. Combined
/// with the pid and a nanosecond clock in the temp filename, this
/// guarantees uniqueness for every writer. The clock alone is not
/// sufficient on macOS: `gettimeofday` there has microsecond
/// resolution, so two threads inside the same process can read the
/// same nanosecond value and collide on a fixed temp name.
///
/// Only used by the test helper below, so it is `cfg(test)`-gated.
#[cfg(test)]
fn next_temp_seq() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmpdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kod-iso-owner-{}-{}-{}",
            std::process::id(),
            next_temp_seq(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_fresh_marker_names_the_current_process() {
        let m = IsolationOwnership::for_current_process("test");
        assert_eq!(m.pid, std::process::id());
        assert!(m.is_alive(), "the current process is trivially alive");
    }

    #[test]
    fn write_and_read_round_trip() {
        let dir = tmpdir();
        write_marker(&dir, "round-trip").unwrap();
        let back = read_marker(&dir).expect("marker");
        assert_eq!(back.pid, std::process::id());
        assert_eq!(back.id, "round-trip");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_pid_that_does_not_exist_is_dead() {
        // A pid very unlikely to exist; if it does, the test is
        // skipped with a note.
        let pid = 4_000_000u32;
        if is_pid_alive(pid) {
            eprintln!("pid {pid} unexpectedly alive; skipping");
            return;
        }
        let m = IsolationOwnership {
            pid,
            id: "x".into(),
            start_token: "0".into(),
        };
        assert!(!m.is_alive());
    }

    #[test]
    fn reap_dead_ignores_a_worktree_with_no_marker() {
        let dir = tmpdir();
        let worktrees = dir.join(".kod").join("worktrees");
        fs::create_dir_all(worktrees.join("bare")).unwrap();
        let dead = reap_dead(&dir).unwrap();
        assert!(dead.is_empty(), "a marker-less worktree is skipped");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn reap_dead_keeps_a_worktree_owned_by_a_live_process() {
        let dir = tmpdir();
        let worktrees = dir.join(".kod").join("worktrees");
        fs::create_dir_all(worktrees.join("live")).unwrap();
        write_marker(&worktrees.join("live"), "self").unwrap();
        let dead = reap_dead(&dir).unwrap();
        assert!(dead.is_empty(), "a live-owned worktree is not reaped");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn reap_dead_returns_a_worktree_owned_by_a_dead_process() {
        let dir = tmpdir();
        let worktrees = dir.join(".kod").join("worktrees");
        let wt = worktrees.join("dead");
        fs::create_dir_all(&wt).unwrap();
        // Write a marker manually with a pid that cannot exist.
        let marker = IsolationOwnership {
            pid: 4_000_000u32,
            id: "gone".into(),
            start_token: "0".into(),
        };
        let body = serde_json::to_string(&marker).unwrap();
        fs::write(wt.join(MARKER_FILENAME), body).unwrap();
        if is_pid_alive(marker.pid) {
            eprintln!("pid {} unexpectedly alive; skipping", marker.pid);
            return;
        }
        let dead = reap_dead(&dir).unwrap();
        assert_eq!(dead.len(), 1, "one dead worktree");
        assert_eq!(dead[0], wt);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_marker_is_idempotent() {
        let dir = tmpdir();
        write_marker(&dir, "x").unwrap();
        remove_marker(&dir).unwrap();
        remove_marker(&dir).unwrap(); // no error when absent
        assert!(read_marker(&dir).is_none());
        let _ = fs::remove_dir_all(&dir);
    }
}
