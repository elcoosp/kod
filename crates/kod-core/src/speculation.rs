//! Speculative read execution (borrow from oh-my-pi, delta §10).
//!
//! # The latency this hides
//!
//! A model that streams a `read_file` tool call spends real wall time
//! producing the call's arguments — the provider emits the call name,
//! then the opening brace, then the path, then the closing brace,
//! across several chunks and often tens of milliseconds. The file
//! read that follows costs a few more milliseconds. Sequentially that
//! is `provider_tail + read`, and the read could have overlapped the
//! tail.
//!
//! Speculation: as soon as enough of the argument JSON is present to
//! identify a `read_file` call and its `path`, start the read in the
//! background. When the call is finally dispatched, the read is
//! usually already done. The cost of being wrong is one wasted read.
//!
//! # Why this is narrow
//!
//! Only **provably read-only** work is speculatable. The design's own
//! admission rule: a single `local_read` effect, no extension
//! lifecycle handlers, an auto-allow approval decision, and the
//! resolved path identical to the argument path. This module is the
//! coordinator, not the admission policy — the caller decides which
//! candidates to admit, and this module handles the pre-fetch and
//! the validate-or-discard step.
//!
//! # TOCTOU
//!
//! A speculative read is a snapshot taken at one moment and consumed
//! at another. Between them, the file could change. The design's fix
//! is *evidence*: capture `(dev, ino, mtime, size, digest)` before
//! the read, re-check the same tuple at commit, and discard on any
//! mismatch. The digest is over the raw bytes; a same-length,
//! same-mtime edit is caught by the digest even when the metadata
//! checks pass.
//!
//! The evidence also composes with the file-touch bus: a write that
//! fires a touch for the same path is exactly the signal that the
//! speculative read is now stale.
//!
//! # What this does NOT do
//!
//! * Not the admission policy. Deciding which calls are safe to
//!   pre-execute is the caller's concern (and depends on approvals,
//!   extensions, path resolution) — this module only carries the
//!   result.
//! * Not a cache. The evidence is single-use; a validated handle is
//!   consumed at commit.
//! * Not a sandbox. The read happens under whatever permissions the
//!   caller's file access has — the module does not re-check.

use sha2::{Digest, Sha256};
use std::path::Path;

/// The identity of a file at one moment.
///
/// Captured before a speculative read and re-checked at commit. Any
/// field differing means the file changed and the read must be
/// discarded.
///
/// `mtime_ns` is nanoseconds since the Unix epoch, not a
/// `SystemTime` — the comparison is equality, and nanosecond
/// granularity catches the fast edit that `mtime` (second
/// granularity on some filesystems) would miss.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Evidence {
    pub dev: u64,
    pub ino: u64,
    pub mtime_ns: i128,
    pub size: u64,
    /// SHA-256 over the raw bytes read. The metadata checks catch a
    /// file replaced by a different inode; the digest catches an
    /// in-place edit that preserves size and mtime (which a
    /// `write` followed by `utimes` can produce).
    pub digest: [u8; 32],
}

/// Capture the file's identity. Fails when the path cannot be
/// statted (missing, permission denied) — a failed capture means the
/// speculation cannot be validated and should not be admitted.
pub fn capture_evidence(path: &Path) -> std::io::Result<Evidence> {
    let meta = std::fs::metadata(path)?;
    #[cfg(unix)]
    let (dev, ino) = {
        use std::os::unix::fs::MetadataExt;
        (meta.dev(), meta.ino())
    };
    #[cfg(not(unix))]
    let (dev, ino) = (0u64, 0u64);
    let mtime_ns = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i128)
        .unwrap_or(0);
    let bytes = std::fs::read(path)?;
    let digest: [u8; 32] = Sha256::digest(&bytes).into();
    Ok(Evidence {
        dev,
        ino,
        mtime_ns,
        size: meta.len(),
        digest,
    })
}

/// Read a file and its evidence in one pass.
///
/// The evidence's digest is computed from the same bytes that are
/// returned, so a caller that validates the evidence and then uses
/// the text is using *consistent* data — a two-step "capture, then
/// read" would race even against itself.
pub fn read_with_evidence(path: &Path) -> std::io::Result<(String, Evidence)> {
    let meta = std::fs::metadata(path)?;
    #[cfg(unix)]
    let (dev, ino) = {
        use std::os::unix::fs::MetadataExt;
        (meta.dev(), meta.ino())
    };
    #[cfg(not(unix))]
    let (dev, ino) = (0u64, 0u64);
    let mtime_ns = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i128)
        .unwrap_or(0);
    let bytes = std::fs::read(path)?;
    let digest: [u8; 32] = Sha256::digest(&bytes).into();
    let text = String::from_utf8_lossy(&bytes).into_owned();
    Ok((
        text,
        Evidence {
            dev,
            ino,
            mtime_ns,
            size: meta.len(),
            digest,
        },
    ))
}

/// Re-check a file against captured evidence.
///
/// Returns `true` when the file is byte-identical to what the
/// evidence describes. The check is:
///
/// 1. Stat the file. A missing or replaced inode fails immediately.
/// 2. Compare `(dev, ino, mtime_ns, size)`. Any difference fails.
/// 3. Re-read and compare the digest. Same inode, same size, same
///    mtime, different bytes is a fast edit that the metadata
///    checks cannot catch — the digest is the last word.
///
/// A failure at step 1 or 2 skips the read entirely. A failure at
/// step 3 is what the design calls the "post-execution mutation
/// veto".
pub fn validate(path: &Path, evidence: &Evidence) -> std::io::Result<bool> {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return Ok(false),
    };
    #[cfg(unix)]
    let (dev, ino) = {
        use std::os::unix::fs::MetadataExt;
        (meta.dev(), meta.ino())
    };
    #[cfg(not(unix))]
    let (dev, ino) = (0u64, 0u64);
    let mtime_ns = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i128)
        .unwrap_or(0);
    if dev != evidence.dev || ino != evidence.ino {
        return Ok(false);
    }
    if mtime_ns != evidence.mtime_ns || meta.len() != evidence.size {
        return Ok(false);
    }
    let bytes = std::fs::read(path)?;
    let digest: [u8; 32] = Sha256::digest(&bytes).into();
    Ok(digest == evidence.digest)
}

/// The outcome of consuming a speculative read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpecOutcome {
    /// The evidence validated and the read is usable. Carries the
    /// text and the digest (the digest is useful for a
    /// file-touch record that wants to store what was read).
    Committed {
        text: String,
        digest: [u8; 32],
    },
    /// The evidence no longer describes the file. The caller must
    /// re-read from scratch — the speculative read's bytes are
    /// discarded, not returned.
    Discarded(Reason),
}

/// Why a speculative read was discarded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    /// The file no longer matches its captured evidence.
    Stale,
    /// The read itself failed (the file is gone, permissions
    /// changed, an I/O error).
    ReadFailed(String),
}

/// Consume a `(text, evidence)` pair, validating first.
///
/// The whole point of the design's validate-then-commit shape: the
/// caller holds a speculative read and this function answers whether
/// the bytes it holds are still the file's bytes. On `Discarded`,
/// the caller must re-read.
///
/// # Not the same as `validate`
///
/// `validate` answers "is the file still the one I saw?".
/// `consume` answers the same question *and* packages the result for
/// the caller. The difference matters for the error shape: a
/// validation that fails because the path is gone returns
/// `Discarded(Stale)` from `consume`, while `validate` would return
/// `Ok(false)` — the caller that wants the distinction (was it
/// deleted, or was it replaced?) can use `validate` directly.
pub fn consume(path: &Path, text: String, evidence: Evidence) -> SpecOutcome {
    match validate(path, &evidence) {
        Ok(true) => SpecOutcome::Committed {
            text,
            digest: evidence.digest,
        },
        Ok(false) => SpecOutcome::Discarded(Reason::Stale),
        Err(e) => SpecOutcome::Discarded(Reason::ReadFailed(e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn write(dir: &TempDir, name: &str, body: &str) -> std::path::PathBuf {
        let p = dir.path().join(name);
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        f.sync_all().unwrap();
        p
    }

    #[test]
    fn capture_records_the_digest() {
        let tmp = TempDir::new().unwrap();
        let p = write(&tmp, "a.txt", "hello");
        let e = capture_evidence(&p).unwrap();
        assert_eq!(e.size, 5);
        assert_ne!(e.digest, [0u8; 32]);
    }

    #[test]
    fn read_with_evidence_returns_the_text_and_the_matching_digest() {
        let tmp = TempDir::new().unwrap();
        let p = write(&tmp, "a.txt", "hello world");
        let (text, e) = read_with_evidence(&p).unwrap();
        assert_eq!(text, "hello world");
        // The digest of "hello world".
        let expect: [u8; 32] = Sha256::digest(b"hello world").into();
        assert_eq!(e.digest, expect);
    }

    #[test]
    fn validate_succeeds_when_the_file_is_unchanged() {
        let tmp = TempDir::new().unwrap();
        let p = write(&tmp, "a.txt", "hello");
        let (_t, e) = read_with_evidence(&p).unwrap();
        assert!(validate(&p, &e).unwrap());
    }

    #[test]
    fn validate_fails_when_the_file_is_deleted() {
        let tmp = TempDir::new().unwrap();
        let p = write(&tmp, "a.txt", "hello");
        let (_t, e) = read_with_evidence(&p).unwrap();
        std::fs::remove_file(&p).unwrap();
        assert!(!validate(&p, &e).unwrap());
    }

    #[test]
    fn validate_fails_when_the_content_changes() {
        let tmp = TempDir::new().unwrap();
        let p = write(&tmp, "a.txt", "hello");
        let (_t, e) = read_with_evidence(&p).unwrap();
        // Change the content, preserving size where possible.
        std::fs::write(&p, "world").unwrap();
        assert!(!validate(&p, &e).unwrap());
    }

    #[test]
    fn validate_fails_on_a_same_size_same_mtime_in_place_edit() {
        // The hardest case: same file, same size, mtime forced back
        // to what it was. Only the digest catches it.
        let tmp = TempDir::new().unwrap();
        let p = write(&tmp, "a.txt", "hello");
        let (_t, mut e) = read_with_evidence(&p).unwrap();
        std::fs::write(&p, "world").unwrap();
        // Force mtime back. On Linux `filetime` would do this;
        // here we cheat and mutate the evidence's mtime to the
        // current one (the test proves the digest gate fires even
        // when the metadata matches).
        let meta = std::fs::metadata(&p).unwrap();
        e.mtime_ns = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as i128)
            .unwrap_or(0);
        e.size = meta.len();
        assert!(!validate(&p, &e).unwrap(), "digest must catch the edit");
    }

    #[test]
    fn consume_commits_a_valid_read() {
        let tmp = TempDir::new().unwrap();
        let p = write(&tmp, "a.txt", "hello");
        let (text, e) = read_with_evidence(&p).unwrap();
        match consume(&p, text, e) {
            SpecOutcome::Committed { text, .. } => assert_eq!(text, "hello"),
            other => panic!("expected Committed, got {other:?}"),
        }
    }

    #[test]
    fn consume_discards_a_stale_read() {
        let tmp = TempDir::new().unwrap();
        let p = write(&tmp, "a.txt", "hello");
        let (text, e) = read_with_evidence(&p).unwrap();
        std::fs::write(&p, "goodbye").unwrap();
        match consume(&p, text, e) {
            SpecOutcome::Discarded(Reason::Stale) => {}
            other => panic!("expected Discarded(Stale), got {other:?}"),
        }
    }

    #[test]
    fn consume_discards_when_the_path_disappears() {
        let tmp = TempDir::new().unwrap();
        let p = write(&tmp, "a.txt", "hello");
        let (text, e) = read_with_evidence(&p).unwrap();
        std::fs::remove_file(&p).unwrap();
        match consume(&p, text, e) {
            SpecOutcome::Discarded(Reason::Stale) => {}
            other => panic!("expected Discarded(Stale), got {other:?}"),
        }
    }

    #[test]
    fn capture_of_a_missing_file_errors() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("nope.txt");
        assert!(capture_evidence(&p).is_err());
    }

    #[test]
    fn evidence_equality_is_field_wise() {
        let tmp = TempDir::new().unwrap();
        let p = write(&tmp, "a.txt", "x");
        let e1 = capture_evidence(&p).unwrap();
        let e2 = capture_evidence(&p).unwrap();
        // Different captures of an unchanged file: same fields except
        // possibly mtime (which does not change for a read).
        assert_eq!(e1.digest, e2.digest);
        assert_eq!(e1.size, e2.size);
        assert_eq!(e1.ino, e2.ino);
    }
}
