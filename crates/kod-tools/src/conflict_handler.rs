//! Delta §7.7 item 4: the `conflict://` resolution loop.
//!
//! # The problem
//!
//! A merge (or a `git stash apply`) leaves a file with conflict
//! markers:
//!
//! ```text
//! <<<<<<< HEAD
//! let x = 1;
//! =======
//! let x = 2;
//! >>>>>>> other
//! ```
//!
//! The model reads the file, sees the markers as text, and has to
//! reconstruct the whole file to resolve one block — wasteful and
//! error-prone. The design's fix: scan the markers, give each block a
//! **stable id**, and let the model resolve a block by writing its
//! replacement to `conflict://<id>`. The handler does the splice.
//!
//! # Stable ids
//!
//! An id is a short hash of `(path, block byte range, block body)`.
//! The same conflict gets the same id across two scans of an unchanged
//! file — so a model that reads, thinks, and writes does not race a
//! re-numbering.
//!
//! # Column-0 strictness
//!
//! Only a `<<<<<<<` at column 0 (no leading whitespace) opens a block.
//! A marker inside a string literal or an indented example is not a
//! conflict; requiring column 0 is the cheap discriminator the design
//! names.
//!
//! # Bounds
//!
//! A file over [`MAX_SCAN_BYTES`] (10 MiB) is not scanned — the read
//! would dominate the turn, and a file that large with conflicts is
//! not a case a model should resolve inline.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::internal_url::{ProtocolError, ProtocolHandler, ResolveContext, ResolvedResource};
use async_trait::async_trait;

/// Files larger than this are not scanned for conflicts.
pub const MAX_SCAN_BYTES: u64 = 10 * 1024 * 1024;

/// One conflict block in a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictBlock {
    /// Stable id: a short hash of `(path, start, body)`.
    pub id: String,
    /// The `<<<<<<<` line's label (the "ours" side), trimmed.
    pub ours_label: String,
    /// The `>>>>>>>` line's label (the "theirs" side), trimmed.
    pub theirs_label: String,
    /// Byte offset of the `<<<<<<<` line's start.
    pub start: usize,
    /// Byte offset just past the `>>>>>>>` line's end (exclusive).
    pub end: usize,
    /// The ours side's text (lines between `<<<<<<<` and `=======`).
    pub ours: String,
    /// The theirs side's text (lines between `=======` and `>>>>>>>`).
    pub theirs: String,
}

/// A registered conflict, addressable by id.
#[derive(Debug, Clone)]
struct Registered {
    path: PathBuf,
    block: ConflictBlock,
    /// The exact bytes at [start, end) when the block was scanned.
    /// `write` re-reads the file and compares against this, so a
    /// same-length external edit cannot shift the splice onto
    /// unrelated text.
    body: String,
}

/// The session-scoped conflict registry. `resolve` scans and registers;
/// `write` splices and deregisters.
#[derive(Debug, Default)]
pub struct ConflictStore {
    entries: Mutex<HashMap<String, Registered>>,
}

impl ConflictStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// The number of registered conflicts. For tests and a debug
    /// surface.
    pub fn len(&self) -> usize {
        self.entries.lock().map(|g| g.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn register(&self, path: &std::path::Path, block: ConflictBlock, body: String) {
        if let Ok(mut g) = self.entries.lock() {
            g.insert(
                block.id.clone(),
                Registered {
                    path: path.to_path_buf(),
                    block,
                    body,
                },
            );
        }
    }

    fn take(&self, id: &str) -> Option<Registered> {
        self.entries.lock().ok()?.remove(id)
    }
}

/// A stable id for a block: FNV-1a over `path | start | body`,
/// rendered as 8 hex chars.
fn stable_id(path: &std::path::Path, start: usize, body: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in path.to_string_lossy().as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    for b in start.to_le_bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    for b in body.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{:08x}", (h ^ (h >> 32)) as u32)
}

/// Scan `text` for conflict blocks. Only column-0 `<<<<<<<` opens a
/// block; a `|||||||` (diff3 base) section is tolerated and dropped.
pub fn scan(text: &str) -> Vec<ConflictBlock> {
    let mut out: Vec<ConflictBlock> = Vec::new();
    let bytes = text.as_bytes();
    let mut line_starts: Vec<usize> = vec![0];
    for (i, b) in bytes.iter().enumerate() {
        if *b == b'\n' {
            line_starts.push(i + 1);
        }
    }
    let line_end = |start: usize| -> usize {
        match text[start..].find('\n') {
            Some(n) => start + n + 1,
            None => text.len(),
        }
    };

    let mut i = 0; // line index
    while i < line_starts.len() {
        let ls = line_starts[i];
        let le = line_end(ls);
        let line = &text[ls..le];
        if line.starts_with("<<<<<<<") {
            let start = ls;
            let ours_label = line.trim_start_matches('<').trim().to_string();
            // Walk forward to `=======`, then `>>>>>>>`.
            let mut ours = String::new();
            let mut theirs = String::new();
            let mut theirs_label = String::new();
            let mut j = i + 1;
            let mut in_theirs = false;
            let mut found_end = false;
            let mut end = le;
            while j < line_starts.len() {
                let js = line_starts[j];
                let je = line_end(js);
                let l = &text[js..je];
                if l.starts_with("|||||||") {
                    // diff3 base section: skip its lines until `=======`.
                    j += 1;
                    while j < line_starts.len() {
                        let ks = line_starts[j];
                        let ke = line_end(ks);
                        if text[ks..ke].starts_with("=======") {
                            break;
                        }
                        j += 1;
                    }
                    continue;
                }
                if l.starts_with("=======") {
                    in_theirs = true;
                    j += 1;
                    continue;
                }
                if l.starts_with(">>>>>>>") {
                    theirs_label = l.trim_start_matches('>').trim().to_string();
                    end = je;
                    found_end = true;
                    break;
                }
                if in_theirs {
                    theirs.push_str(l);
                } else {
                    ours.push_str(l);
                }
                j += 1;
            }
            if found_end {
                let body = &text[start..end];
                out.push(ConflictBlock {
                    id: stable_id(std::path::Path::new(""), start, body),
                    ours_label,
                    theirs_label,
                    start,
                    end,
                    ours,
                    theirs,
                });
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// The `conflict://` handler.
pub struct ConflictHandler {
    store: Arc<ConflictStore>,
}

impl ConflictHandler {
    pub fn new(store: Arc<ConflictStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl ProtocolHandler for ConflictHandler {
    fn scheme(&self) -> &'static str {
        "conflict"
    }

    async fn resolve(
        &self,
        url: &str,
        ctx: &ResolveContext,
    ) -> std::result::Result<ResolvedResource, ProtocolError> {
        let rest = url
            .strip_prefix("conflict://")
            .ok_or_else(|| ProtocolError::Malformed {
                url: url.to_string(),
                reason: "not a conflict:// url".to_string(),
            })?;
        // `conflict://<id>` resolves one block; `conflict://<path>`
        // scans a file and lists its blocks.
        if let Ok(g) = self.store.entries.lock()
            && let Some(reg) = g.get(rest)
        {
            let b = &reg.block;
            let body = format!(
                "conflict {} in {}\n\
                 <<<<<<< {}\n{}=======\n{}>>>>>>> {}\n\n\
                 Resolve it with: write conflict://{} with the final text \
                 for this block (the markers are removed for you).",
                b.id,
                reg.path.display(),
                b.ours_label,
                b.ours,
                b.theirs,
                b.theirs_label,
                b.id,
            );
            return Ok(ResolvedResource {
                text: body,
                mime: Some("text/plain".to_string()),
                immutable: false,
            });
        }
        // Treat `rest` as a path.
        let path = if std::path::Path::new(rest).is_absolute() {
            PathBuf::from(rest)
        } else {
            ctx.working_dir.join(rest)
        };
        let meta = std::fs::metadata(&path).map_err(|e| ProtocolError::Handler {
            url: url.to_string(),
            message: e.to_string(),
        })?;
        if meta.len() > MAX_SCAN_BYTES {
            return Err(ProtocolError::Handler {
                url: url.to_string(),
                message: format!(
                    "file is {} bytes, over the {} byte conflict-scan cap",
                    meta.len(),
                    MAX_SCAN_BYTES,
                ),
            });
        }
        let text = std::fs::read_to_string(&path).map_err(|e| ProtocolError::Handler {
            url: url.to_string(),
            message: e.to_string(),
        })?;
        let blocks = scan(&text);
        if blocks.is_empty() {
            return Ok(ResolvedResource {
                text: format!("no conflict markers in {}", path.display()),
                mime: Some("text/plain".to_string()),
                immutable: false,
            });
        }
        let mut listing = format!("{} conflict(s) in {}:\n\n", blocks.len(), path.display());
        for b in &blocks {
            // Re-key the id on the real path so it matches a later
            // `conflict://<id>` resolve and the `write` splice.
            let body = &text[b.start..b.end];
            let id = stable_id(&path, b.start, body);
            let mut registered = b.clone();
            registered.id = id.clone();
            self.store
                .register(&path, registered, body.to_string());
            listing.push_str(&format!(
                "conflict://{id}\n  ours ({}): {} line(s)\n  theirs ({}): {} line(s)\n\n",
                b.ours_label,
                b.ours.lines().count(),
                b.theirs_label,
                b.theirs.lines().count(),
            ));
        }
        Ok(ResolvedResource {
            text: listing,
            mime: Some("text/plain".to_string()),
            immutable: false,
        })
    }

    async fn write(
        &self,
        url: &str,
        content: &str,
        _ctx: &ResolveContext,
    ) -> std::result::Result<(), ProtocolError> {
        let id = url.strip_prefix("conflict://").ok_or_else(|| {
            ProtocolError::Malformed {
                url: url.to_string(),
                reason: "not a conflict:// url".to_string(),
            }
        })?;
        let Some(reg) = self.store.take(id) else {
            return Err(ProtocolError::Handler {
                url: url.to_string(),
                message: format!("unknown or already-resolved conflict {id}"),
            });
        };
        let text = std::fs::read_to_string(&reg.path).map_err(|e| ProtocolError::Handler {
            url: url.to_string(),
            message: e.to_string(),
        })?;
        let b = &reg.block;
        if b.end > text.len() || b.start > b.end {
            return Err(ProtocolError::Malformed {
                url: url.to_string(),
                reason: "registered block no longer fits the file (it changed)".to_string(),
            });
        }
        // F2f-22: byte-length is not integrity — a same-length edit
        // since the scan shifts every registered offset onto unrelated
        // text. Compare the bytes now at [start, end) against the body
        // recorded at scan time.
        let current_body = &text[b.start..b.end];
        if current_body != reg.body {
            return Err(ProtocolError::Malformed {
                url: url.to_string(),
                reason: "registered conflict block changed since the scan".to_string(),
            });
        }
        // The replacement is the caller's text, made to end with a
        // newline so the splice does not merge lines.
        let mut replacement = content.to_string();
        if !replacement.ends_with('\n') {
            replacement.push('\n');
        }
        let mut new_text = String::with_capacity(text.len());
        new_text.push_str(&text[..b.start]);
        new_text.push_str(&replacement);
        new_text.push_str(&text[b.end..]);
        std::fs::write(&reg.path, new_text).map_err(|e| ProtocolError::Handler {
            url: url.to_string(),
            message: e.to_string(),
        })?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_finds_one_block() {
        let text = "before\n<<<<<<< HEAD\nlet x = 1;\n=======\nlet x = 2;\n>>>>>>> other\nafter\n";
        let blocks = scan(text);
        assert_eq!(blocks.len(), 1);
        let b = &blocks[0];
        assert_eq!(b.ours_label, "HEAD");
        assert_eq!(b.theirs_label, "other");
        assert_eq!(b.ours, "let x = 1;\n");
        assert_eq!(b.theirs, "let x = 2;\n");
    }

    #[test]
    fn scan_ignores_indented_markers() {
        // A marker inside an example (indented) is not a conflict.
        let text = "  <<<<<<< HEAD\n  let x = 1;\n  =======\n  >>>>>>> other\n";
        assert!(scan(text).is_empty());
    }

    #[test]
    fn scan_handles_diff3_base_section() {
        let text = "<<<<<<< HEAD\na\n||||||| base\nb\n=======\nc\n>>>>>>> other\n";
        let blocks = scan(text);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].ours, "a\n");
        assert_eq!(blocks[0].theirs, "c\n");
    }

    #[test]
    fn scan_finds_two_blocks() {
        let text = "\
<<<<<<< A
1
=======
2
>>>>>>> B
mid
<<<<<<< A
3
=======
4
>>>>>>> B
";
        assert_eq!(scan(text).len(), 2);
    }

    #[test]
    fn ids_are_stable_across_scans() {
        let text = "<<<<<<< HEAD\na\n=======\nb\n>>>>>>> other\n";
        let a = scan(text);
        let b = scan(text);
        assert_eq!(a[0].id, b[0].id, "same conflict, same id");
    }

    #[test]
    fn ids_differ_for_different_blocks() {
        let t1 = "<<<<<<< HEAD\na\n=======\nb\n>>>>>>> other\n";
        let t2 = "<<<<<<< HEAD\nc\n=======\nd\n>>>>>>> other\n";
        assert_ne!(scan(t1)[0].id, scan(t2)[0].id);
    }

    #[test]
    fn resolve_lists_then_write_splices() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("c.txt");
        std::fs::write(
            &f,
            "before\n<<<<<<< HEAD\nold\n=======\nnew\n>>>>>>> other\nafter\n",
        )
        .unwrap();
        let store = Arc::new(ConflictStore::new());
        let h = ConflictHandler::new(Arc::clone(&store));
        let ctx = ResolveContext::new("", tmp.path());
        let rt = tokio::runtime::Runtime::new().unwrap();

        // Resolve by path: scans and lists.
        let listing = rt
            .block_on(h.resolve("conflict://c.txt", &ctx))
            .expect("resolve");
        assert!(listing.text.contains("conflict://"), "got: {}", listing.text);
        assert_eq!(store.len(), 1);

        // Grab the id and write a resolution.
        let id = {
            let g = store.entries.lock().unwrap();
            g.keys().next().unwrap().clone()
        };
        rt.block_on(h.write(&format!("conflict://{id}"), "resolved", &ctx))
            .expect("write");

        let after = std::fs::read_to_string(&f).unwrap();
        assert_eq!(after, "before\nresolved\nafter\n");
        assert!(store.is_empty(), "resolved conflict is deregistered");
    }

    #[test]
    fn writing_an_unknown_id_is_an_error() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = Arc::new(ConflictStore::new());
        let h = ConflictHandler::new(store);
        let ctx = ResolveContext::new("", tmp.path());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let e = rt.block_on(h.write("conflict://deadbeef", "x", &ctx));
        assert!(e.is_err());
    }

    #[test]
    fn a_file_with_no_conflicts_reports_none() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("clean.txt");
        std::fs::write(&f, "no conflicts here\n").unwrap();
        let store = Arc::new(ConflictStore::new());
        let h = ConflictHandler::new(store);
        let ctx = ResolveContext::new("", tmp.path());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let r = rt
            .block_on(h.resolve("conflict://clean.txt", &ctx))
            .expect("resolve");
        assert!(r.text.contains("no conflict markers"), "got: {}", r.text);
    }
}
