//! Shared helpers used by more than one module in this crate.
//!
//! Extracted from `tools.rs` so the next tool split (WriteFileTool,
//! which uses `atomic_write`) can move its own impl to a module
//! without dragging these helpers along.

use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};

/// Monotonic process-local counter used to break ties between
/// concurrent atomic-replace writers in the same process. Same shape
/// as the helpers in `kod-core-state`, `kod-config`, `kod-mcp`, and
/// `kod-tui`.
pub(crate) fn next_temp_seq() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}
/// H-R13: atomic file write. `std::fs::write` truncates in place; a
/// crash or ENOSPC mid-write leaves a torn file that the engine then
/// feeds back to the model. A same-directory temp file + fsync + rename
/// is atomic on POSIX (and on Windows, `rename` over an existing file
/// is atomic since Rust 1.55).
///
/// The temp file name is `.kod-tmp-<pid>-<nanos>` in the destination
/// directory (same filesystem — a cross-fs rename is a copy + delete,
/// not atomic). On any error the temp file is removed.
pub(crate) fn atomic_write(path: &std::path::Path, content: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // Unique temp per process + a monotonic in-process counter +
    // nanosecond clock. The pre-fix shape used only pid + nanos,
    // which is not enough on macOS: `gettimeofday` has microsecond
    // resolution, so two threads in the same process writing to the
    // same directory can read the same nanosecond value and collide
    // on the temp name. The rename then fails with ENOENT (the peer
    // already renamed the temp away), and the second write is lost.
    // Same shape as the tool_cache / state / persistence fixes.
    let seq = next_temp_seq();
    let tmp = parent.join(format!(".kod-tmp-{}-{seq}-{nanos:x}", std::process::id(),));
    // F2f-17: the rename replaces the inode, so the destination's
    // mode (notably the exec bit on a script overwrite) would be lost
    // — the temp file's default 0644 would win. Capture the existing
    // destination's permissions and reapply them to the temp file
    // before the rename. A destination that does not exist yet keeps
    // the platform default.
    let dest_perms = std::fs::metadata(path).ok().map(|m| m.permissions());
    let result = (|| -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(content)?;
        f.sync_all()?;
        drop(f);
        if let Some(perms) = dest_perms {
            // Best-effort: a filesystem that refuses set_permissions
            // still gets the content, which is the point of the write.
            let _ = std::fs::set_permissions(&tmp, perms);
        }
        std::fs::rename(&tmp, path)?;
        // T1-C6: fsync the parent directory so the rename is durable.
        if let Some(parent) = path.parent()
            && let Ok(dir) = std::fs::File::open(parent)
        {
            let _ = dir.sync_all();
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    } else {
        // Delta §7.7: a successful write invalidates the walker
        // cache. A subsequent `list_files`/`grep` in the same turn
        // must see the file that was just created.
        crate::walk_cache::invalidate_all();
    }
    result
}
/// W14: signal the whole process group, not just the direct child.
/// `execute_command` spawns `sh -c <command>`; a shell that forks
/// (dash -c "sleep 60") leaves the grandchild holding the stdout /
/// stderr write-ends, so killing only the shell left the drain loop
/// blocked until the grandchild exited on its own. With the child in
/// its own process group (`process_group(0)`), `kill(-pgid)` reaches
/// the tree.
pub(crate) fn kill_child_tree(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        // T1-C8: signal the session, not just the process group. A
        // child can setpgid(0, 0) to escape a pgroup kill; a session
        // created by setsid(2) cannot be left — the SID is invariant.
        let sid = unsafe { libc::getsid(pid as libc::pid_t) };
        if sid > 0 && sid == pid as libc::pid_t {
            let rc = unsafe { libc::kill(-sid, libc::SIGKILL) };
            if rc == 0 {
                return;
            }
        }
        let pgid = -(pid as libc::pid_t);
        let rc = unsafe { libc::kill(pgid, libc::SIGKILL) };
        if rc == 0 {
            return;
        }
    }
    // Non-unix, or the session kill failed: fall back to the direct child.
    let _ = child.start_kill();
}
