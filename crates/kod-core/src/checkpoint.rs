//! File checkpoint system.
//!
//! Before a mutating tool (`write_file`, `patch_file`) overwrites or
//! patches a file, its content is captured here. A user who does not
//! like the result can `kod checkpoint restore <n>` or `/rollback` to
//! undo the edit without leaving the session.
//!
//! # Storage layout
//!
//! `~/.kod/checkpoints/<project-hash>/<id>.json` — one JSON file per
//! snapshot. `<project-hash>` is an FNV-1a hash of the canonicalized
//! working directory, so two projects do not see each other's
//! checkpoints and neither pollutes the other's working tree with a
//! `.kod/` directory.
//!
//! `<id>` is `<unix-ms zero-padded>-<in-process counter>`, which sorts
//! lexicographically in the same order as it sorts chronologically —
//! `list` relies on that for "newest first" without reading every
//! file's metadata.
//!
//! # Retention
//!
//! At most [`DEFAULT_MAX_SNAPSHOTS`] files are kept per project;
//! `snapshot_before` enforces the cap by dropping the oldest after
//! every successful write. A user who wants everything deleted runs
//! `kod checkpoint clear`.
//!
//! # Why full content, not diffs
//!
//! A diff of every intermediate state accumulates: N edits to one file
//! produce N patches whose application order and offsets matter. The
//! full content of each prior version is self-contained: a restore is
//! a single `write`, with no dependency on any other snapshot having
//! survived. The storage cost is bounded by the retention cap and the
//! per-file ceiling ([`MAX_SNAPSHOT_BYTES`]).
//!
//! # What it does not do
//!
//! It does not snapshot `execute_command`. A shell command can touch an
//! unbounded set of files (a `cargo fix`, a `sed -i` loop, a build
//! script) and predicting the write set would require the tool itself
//! to have declared it. `execute_command` already runs under the same
//! permissions gate as the other tools; a user who wants a full
//! filesystem-level undo should use git (`git stash`, branches).

use kod_error::{KodError, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Default per-project snapshot cap. Large enough that a full session
/// is almost always recallable, small enough that the directory does
/// not grow without bound on a long-running project.
pub const DEFAULT_MAX_SNAPSHOTS: usize = 200;

/// Per-file snapshot ceiling. A file larger than this is not
/// snapshotted — the checkpoint would dominate the directory, and a
/// file that large is almost always generated output (a lockfile, a
/// bundled artifact) rather than a source file the user wants undone.
const MAX_SNAPSHOT_BYTES: usize = 5 * 1024 * 1024;

/// One saved snapshot. Serialized whole to disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    /// Opaque identifier, also the file name's stem.
    pub id: String,
    /// The file path the tool was about to write, canonicalized at
    /// capture time when the file existed.
    pub path: PathBuf,
    /// The tool that triggered the snapshot (`write_file`, `patch_file`).
    pub tool: String,
    /// Unix milliseconds when the snapshot was taken.
    pub taken_at_ms: u64,
    /// True when the file existed at capture time; false when the tool
    /// was about to create it. Restoring a `false` snapshot deletes
    /// the file.
    pub existed: bool,
    /// The original content, when `existed`. Empty otherwise.
    pub content: String,
}

/// Manages a directory of snapshots for one project.
pub struct CheckpointManager {
    dir: PathBuf,
    counter: AtomicU64,
    max_snapshots: usize,
}

impl CheckpointManager {
    /// Open (or create-on-demand) a manager rooted at `dir`.
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            counter: AtomicU64::new(0),
            max_snapshots: DEFAULT_MAX_SNAPSHOTS,
        }
    }

    /// The default manager for `working_dir`:
    /// `~/.kod/checkpoints/<fnv1a-hash-of-canonical-working-dir>/`.
    /// Returns `None` when the home directory cannot be determined
    /// (a stripped container, a test).
    pub fn for_working_dir(working_dir: &Path) -> Option<Self> {
        let home = dirs::home_dir()?;
        let canonical = std::fs::canonicalize(working_dir)
            .unwrap_or_else(|_| working_dir.to_path_buf());
        let hash = fnv1a_hex(canonical.to_string_lossy().as_ref());
        let dir = home.join(".kod").join("checkpoints").join(hash);
        Some(Self::new(dir))
    }

    /// The directory snapshots are stored in.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Override the retention cap (used by tests).
    pub fn with_max_snapshots(mut self, n: usize) -> Self {
        self.max_snapshots = n.max(1);
        self
    }

    /// Snapshot the current content of `path` before a mutating tool
    /// runs. Returns the new snapshot's id, or `None` when there is
    /// nothing useful to snapshot (binary file, oversized file, or a
    /// read error that is not `NotFound`).
    ///
    /// A `NotFound` is *not* a skip: it is a snapshot of "this file did
    /// not exist yet", which a later restore uses to delete the file
    /// the tool is about to create.
    pub fn snapshot_before(&self, path: &Path, tool: &str) -> Result<Option<String>> {
        std::fs::create_dir_all(&self.dir).map_err(KodError::Io)?;

        let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());

        let (existed, content) = match std::fs::read_to_string(&canonical) {
            Ok(s) => (true, s),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (false, String::new()),
            Err(e) if e.kind() == std::io::ErrorKind::IsADirectory => (false, String::new()),
            Err(e) => {
                // Binary file (InvalidData), permission denied, or any
                // other read error. Nothing actionable to save; the tool
                // itself will report the failure if it matters.
                tracing::debug!(
                    path = %canonical.display(),
                    error = %e,
                    "cannot snapshot; skipping"
                );
                return Ok(None);
            }
        };

        if content.len() > MAX_SNAPSHOT_BYTES {
            tracing::warn!(
                path = %canonical.display(),
                bytes = content.len(),
                cap = MAX_SNAPSHOT_BYTES,
                "file too large to snapshot; skipping"
            );
            return Ok(None);
        }

        let ts_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        let id = format!("{ts_ms:013}-{n:04}");

        let snapshot = Snapshot {
            id: id.clone(),
            path: canonical,
            tool: tool.to_string(),
            taken_at_ms: ts_ms,
            existed,
            content,
        };
        let file = self.dir.join(format!("{id}.json"));
        let raw = serde_json::to_vec_pretty(&snapshot)
            .map_err(|e| KodError::Serialization(e.to_string()))?;
        std::fs::write(&file, raw).map_err(KodError::Io)?;

        let _ = self.enforce_retention();

        Ok(Some(id))
    }

    /// Every snapshot for this project, newest first. Unreadable files
    /// are skipped with a warning rather than aborting the listing.
    pub fn list(&self) -> Result<Vec<Snapshot>> {
        if !self.dir.is_dir() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&self.dir).map_err(KodError::Io)? {
            let entry = entry.map_err(KodError::Io)?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let raw = match std::fs::read_to_string(&path) {
                Ok(s) => s,
                Err(_) => continue,
            };
            match serde_json::from_str::<Snapshot>(&raw) {
                Ok(s) => out.push(s),
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "unreadable checkpoint; skipping"
                    );
                }
            }
        }
        // Filenames are `<zero-padded-ts>-<counter>.json`, so a lexical
        // descending sort is a chronological descending sort without
        // touching any file's mtime.
        out.sort_by(|a, b| b.id.cmp(&a.id));
        Ok(out)
    }

    /// Look up a snapshot by id.
    pub fn find(&self, id: &str) -> Result<Option<Snapshot>> {
        let path = self.dir.join(format!("{id}.json"));
        if !path.exists() {
            return Ok(None);
        }
        let raw = std::fs::read_to_string(&path).map_err(KodError::Io)?;
        let s: Snapshot = serde_json::from_str(&raw)
            .map_err(|e| KodError::Deserialization(e.to_string()))?;
        Ok(Some(s))
    }

    /// Write the snapshot's content back to its original path. Returns
    /// the path that was restored. When the snapshot recorded a
    /// non-existent file, the restore deletes whatever is there now.
    pub fn restore(&self, id: &str) -> Result<PathBuf> {
        let s = self.find(id)?.ok_or_else(|| KodError::InvalidParameters {
            reason: format!("no checkpoint with id {id:?}"),
        })?;
        if s.existed {
            if let Some(parent) = s.path.parent()
                && !parent.as_os_str().is_empty()
            {
                std::fs::create_dir_all(parent).map_err(KodError::Io)?;
            }
            std::fs::write(&s.path, s.content.as_bytes()).map_err(KodError::Io)?;
        } else {
            // The tool was about to create this file. Rolling back
            // means removing it — and tolerating its absence, because a
            // tool that failed before writing leaves nothing to delete.
            match std::fs::remove_file(&s.path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(KodError::Io(e)),
            }
        }
        Ok(s.path)
    }

    /// Delete every snapshot for this project. Returns the count removed.
    pub fn clear(&self) -> Result<usize> {
        if !self.dir.is_dir() {
            return Ok(0);
        }
        let mut n = 0;
        for entry in std::fs::read_dir(&self.dir).map_err(KodError::Io)? {
            let entry = entry.map_err(KodError::Io)?;
            if entry.path().extension().and_then(|s| s.to_str()) == Some("json")
                && std::fs::remove_file(entry.path()).is_ok()
            {
                n += 1;
            }
        }
        Ok(n)
    }

    /// Trim the directory to `max_snapshots`, dropping the oldest.
    /// Best-effort: a failed removal is logged, not propagated.
    fn enforce_retention(&self) -> Result<()> {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(&self.dir)
            .map_err(KodError::Io)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
            .collect();
        if entries.len() <= self.max_snapshots {
            return Ok(());
        }
        entries.sort();
        let to_drop = entries.len() - self.max_snapshots;
        for p in entries.into_iter().take(to_drop) {
            if let Err(e) = std::fs::remove_file(&p) {
                tracing::warn!(path = %p.display(), error = %e, "could not drop old checkpoint");
            }
        }
        Ok(())
    }
}

/// FNV-1a 64-bit, rendered as 16 hex chars. Deterministic across runs
/// and processes, which `std::collections::hash_map::DefaultHasher` is
/// not — the checkpoint directory for a project must be the same for
/// the session that wrote a snapshot and the session that restores it.
fn fnv1a_hex(s: &str) -> String {
    const OFFSET: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;
    let mut h = OFFSET;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(PRIME);
    }
    format!("{h:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn mgr() -> (TempDir, CheckpointManager) {
        let tmp = TempDir::new().unwrap();
        let cp = CheckpointManager::new(tmp.path().join("checkpoints"));
        (tmp, cp)
    }

    #[test]
    fn snapshot_before_missing_file_records_create() {
        let (_tmp, cp) = mgr();
        let target = _tmp.path().join("does-not-exist.txt");
        let id = cp
            .snapshot_before(&target, "write_file")
            .unwrap()
            .expect("a non-existent path is still worth recording");
        let s = cp.find(&id).unwrap().unwrap();
        assert!(!s.existed);
        assert!(s.content.is_empty());
        assert_eq!(s.tool, "write_file");
    }

    #[test]
    fn snapshot_before_existing_file_records_content() {
        let (tmp, cp) = mgr();
        let target = tmp.path().join("existing.txt");
        std::fs::write(&target, "original content").unwrap();
        let id = cp.snapshot_before(&target, "patch_file").unwrap().unwrap();
        let s = cp.find(&id).unwrap().unwrap();
        assert!(s.existed);
        assert_eq!(s.content, "original content");
    }

    #[test]
    fn restore_rewrites_original_content() {
        let (tmp, cp) = mgr();
        let target = tmp.path().join("file.txt");
        std::fs::write(&target, "before").unwrap();
        let id = cp.snapshot_before(&target, "write_file").unwrap().unwrap();

        // The tool "runs": the file now contains new content.
        std::fs::write(&target, "after").unwrap();

        let restored = cp.restore(&id).unwrap();
        assert_eq!(restored, std::fs::canonicalize(&target).unwrap());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "before");
    }

    #[test]
    fn restore_deletes_file_that_did_not_exist() {
        let (tmp, cp) = mgr();
        let target = tmp.path().join("created-by-tool.txt");
        // Snapshot before the file exists — a "was created" record.
        let id = cp.snapshot_before(&target, "write_file").unwrap().unwrap();
        // The tool "runs": the file now exists.
        std::fs::write(&target, "new").unwrap();

        cp.restore(&id).unwrap();
        assert!(
            !target.exists(),
            "restore of a create snapshot must delete the file"
        );
    }

    #[test]
    fn restore_missing_target_is_tolerated() {
        // Same as above, but nothing was written after the snapshot —
        // the restore must not error on the already-absent file.
        let (tmp, cp) = mgr();
        let target = tmp.path().join("never-created.txt");
        let id = cp.snapshot_before(&target, "write_file").unwrap().unwrap();
        cp.restore(&id).unwrap();
        assert!(!target.exists());
    }

    #[test]
    fn list_is_newest_first() {
        let (tmp, cp) = mgr();
        let a = tmp.path().join("a.txt");
        std::fs::write(&a, "a").unwrap();
        let id1 = cp.snapshot_before(&a, "write_file").unwrap().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let b = tmp.path().join("b.txt");
        std::fs::write(&b, "b").unwrap();
        let id2 = cp.snapshot_before(&b, "write_file").unwrap().unwrap();

        let listed = cp.list().unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].id, id2, "newest must be first");
        assert_eq!(listed[1].id, id1);
    }

    #[test]
    fn retention_drops_oldest_beyond_cap() {
        let (tmp, cp) = mgr();
        let cp = cp.with_max_snapshots(3);
        let target = tmp.path().join("f.txt");
        std::fs::write(&target, "v0").unwrap();
        for i in 0..5 {
            std::fs::write(&target, format!("v{i}")).unwrap();
            cp.snapshot_before(&target, "write_file").unwrap();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let listed = cp.list().unwrap();
        assert_eq!(listed.len(), 3, "cap must hold, got {}", listed.len());
    }

    #[test]
    fn clear_removes_everything() {
        let (tmp, cp) = mgr();
        let target = tmp.path().join("f.txt");
        std::fs::write(&target, "x").unwrap();
        cp.snapshot_before(&target, "write_file").unwrap();
        cp.snapshot_before(&target, "write_file").unwrap();
        assert_eq!(cp.list().unwrap().len(), 2);
        let removed = cp.clear().unwrap();
        assert_eq!(removed, 2);
        assert!(cp.list().unwrap().is_empty());
    }

    #[test]
    fn restore_unknown_id_errors() {
        let (_tmp, cp) = mgr();
        let err = cp.restore("no-such-id").unwrap_err();
        match err {
            KodError::InvalidParameters { reason } => {
                assert!(reason.contains("no checkpoint"), "got: {reason}");
            }
            other => panic!("expected InvalidParameters, got {other:?}"),
        }
    }

    #[test]
    fn for_working_dir_is_stable_across_calls() {
        let tmp = TempDir::new().unwrap();
        let a = CheckpointManager::for_working_dir(tmp.path()).map(|m| m.dir().to_path_buf());
        let b = CheckpointManager::for_working_dir(tmp.path()).map(|m| m.dir().to_path_buf());
        assert_eq!(a, b, "hash must be deterministic across calls");
    }

    #[test]
    fn fnv1a_hex_is_deterministic() {
        assert_eq!(fnv1a_hex("hello"), fnv1a_hex("hello"));
        assert_ne!(fnv1a_hex("hello"), fnv1a_hex("world"));
        assert_eq!(fnv1a_hex("").len(), 16);
    }

    #[test]
    fn oversized_file_is_skipped() {
        let (tmp, cp) = mgr();
        let target = tmp.path().join("huge.txt");
        // One byte over the cap; the file itself only needs to exist
        // for its size to be checked.
        let big = "x".repeat(MAX_SNAPSHOT_BYTES + 1);
        std::fs::write(&target, &big).unwrap();
        let id = cp.snapshot_before(&target, "write_file").unwrap();
        assert!(id.is_none(), "oversized files must be skipped");
    }
}
