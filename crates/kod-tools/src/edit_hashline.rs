//! Delta §7.1: hashline edit mode — line-addressed edits with tag guards.
//!
//! # The bug this kills
//!
//! `patch_file` and `write_file` address text by string: a model sends
//! the `old_str` it *remembers* and the edit fails (or, worse,
//! matches the wrong place) when the file has changed since the model
//! read it. The model then re-reads, re-guesses, and burns a round.
//!
//! Hashline addresses **lines** and guards on a **tag**: a 4-hex hash
//! of the file as the model last saw it. `read` emits `N:line` rows
//! and records a snapshot. An edit carries `[path#TAG]`; if the tag
//! does not match the file's current tag, the edit is rejected as
//! stale before any change. The model re-reads and retries — never a
//! silent wrong edit.
//!
//! # Second guard: unseen anchors
//!
//! A model may edit a line it never read (it guessed the file's
//! shape). The snapshot records which line numbers the model actually
//! saw; an edit whose anchor line is not in that set is rejected
//! (`UnseenAnchor`). Together the two guards make "the model edits
//! something it did not see" impossible by construction.
//!
//! # Operations (this module)
//!
//! ```text
//! [src/main.rs#a1b2]
//! PUT 12.=12:
//! +new line 12
//! CUT 20.=22
//! PUT >$:
//! +appended
//! ```
//!
//! * `PUT N.=M:` — replace lines N..M (inclusive) with the `+` rows
//!   that follow.
//! * `CUT N.=M` — remove lines N..M.
//! * `PUT >$:` — append at end of file.
//!
//! All-or-nothing: the ops are staged against the snapshot text and
//! written only if every one validates.
//!
//! # What this does NOT do (yet)
//!
//! * No `MV` (move), no `@` clipboard registers. Those are the
//!   design's later ops; the two guards above are the safety core.
//! * No tree-sitter block-context elision in the read footer. Plain
//!   `N:line` output plus an elision footer for a long file.
//! * The tag is a 2-byte (4-hex) hash. The design's own value; short
//!   enough to type, long enough that an accidental collision across
//!   a session is not a real risk.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// 4-hex content tag. Two bytes, rendered `{:04x}`.
pub type Tag = [u8; 2];

/// A file as the model last saw it.
#[derive(Debug, Clone)]
pub struct Snapshot {
    /// The tag `read` reported. An edit must carry this exact value.
    pub tag: Tag,
    /// The file's content when the snapshot was taken.
    pub text: String,
    /// Line numbers (1-based) the model actually saw. An edit whose
    /// anchor is outside this set is rejected.
    pub seen: Vec<bool>,
}

/// Why an edit was rejected. Every variant is a caller-visible error,
/// never a silent no-op.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditError {
    /// No snapshot for this path — the model never read it (or the
    /// session forgot it).
    NeverRead,
    /// The file changed since the snapshot was taken.
    StaleTag { expected: Tag, got: Tag },
    /// An op addresses a line the model did not read.
    UnseenAnchor { line: usize },
    /// A line number is past the end of the file.
    OutOfRange { line: usize, len: usize },
    /// An op's `s` is greater than its `e`.
    BadRange { s: usize, e: usize },
    /// Two ops address overlapping line ranges — applying both would
    /// splice against shifted indices.
    Overlap { first: (usize, usize), second: (usize, usize) },
    /// The op text did not parse.
    Malformed(String),
}

impl std::fmt::Display for EditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EditError::NeverRead => write!(f, "file was never read this session; read it first"),
            EditError::StaleTag { expected, got } => write!(
                f,
                "stale edit: file is #{}, your header says #{}; re-read and retry",
                tag_hex(*expected),
                tag_hex(*got),
            ),
            EditError::UnseenAnchor { line } => {
                write!(f, "edit anchor line {line} was never read; read it first")
            }
            EditError::OutOfRange { line, len } => {
                write!(f, "line {line} is past the end of the file ({len} lines)")
            }
            EditError::BadRange { s, e } => write!(f, "bad range {s}..{e} (start > end)"),
            EditError::Overlap { first, second } => write!(
                f,
                "overlapping ops {}..{} and {}..{} — one range per line",
                first.0, first.1, second.0, second.1
            ),
            EditError::Malformed(m) => write!(f, "malformed edit: {m}"),
        }
    }
}

/// One validated operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Op {
    /// First line of the affected range (1-based, inclusive). For an
    /// append, `s == e == 0` is the sentinel.
    pub start: usize,
    /// Last line of the affected range (1-based, inclusive).
    pub end: usize,
    /// Replacement lines. Empty for a pure `CUT`.
    pub payload: Vec<String>,
    /// `true` for `PUT >$:` (append at end).
    pub append: bool,
}

/// Compute the 4-hex tag for `text`. A short FNV-1a over the bytes,
/// folded to two bytes. Stable across runs and platforms.
pub fn tag_of(text: &str) -> Tag {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    // Fold the high and low halves so a change anywhere moves the tag.
    let folded = (h ^ (h >> 32)) as u16;
    folded.to_be_bytes()
}

/// Render a [`Tag`] as the 4-hex string a header carries.
pub fn tag_hex(tag: Tag) -> String {
    format!("{:02x}{:02x}", tag[0], tag[1])
}

/// The per-session edit store.
#[derive(Debug, Default)]
pub struct EditStore {
    snapshots: HashMap<PathBuf, Snapshot>,
}

impl EditStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the snapshot a `read` produced. `seen` is the 1-based
    /// line-number bit set; `seen[0]` is unused (line numbering starts
    /// at 1).
    pub fn record_snapshot(&mut self, path: &Path, text: &str, seen: Vec<bool>) -> Tag {
        let tag = tag_of(text);
        let key = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        self.snapshots.insert(
            key,
            Snapshot {
                tag,
                text: text.to_string(),
                seen,
            },
        );
        tag
    }

    /// The tag for `path`'s current snapshot, if any.
    pub fn current_tag(&self, path: &Path) -> Option<Tag> {
        let key = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        self.snapshots.get(&key).map(|s| s.tag)
    }

    /// Forget a snapshot (after a successful edit the tag is stale).
    pub fn forget(&mut self, path: &Path) {
        let key = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        self.snapshots.remove(&key);
    }

    /// Apply `ops` to `path` under the tag guard. Returns the new tag
    /// on success (the model chains edits on it). All-or-nothing:
    /// nothing is written if any op fails validation.
    pub fn apply(
        &mut self,
        path: &Path,
        tag: Tag,
        ops: &[Op],
    ) -> Result<(Tag, String), EditError> {
        let key = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let snap = self.snapshots.get(&key).ok_or(EditError::NeverRead)?;
        if snap.tag != tag {
            return Err(EditError::StaleTag {
                expected: snap.tag,
                got: tag,
            });
        }
        // Guard against the FILE, not just the snapshot: an external
        // edit since the read (sed -i, a git op, a peer) would otherwise
        // be silently clobbered by the staged splice.
        let current = std::fs::read_to_string(path).map_err(|e| {
            EditError::Malformed(format!("re-read before edit failed: {e}"))
        })?;
        // W12: compare exact bytes, not the 16-bit tag. `tag_of` folds
        // FNV-1a to two bytes; two different contents can collide
        // (1/65536), and a collision here would green-light a splice
        // against text the model never saw. The snapshot text is
        // already in memory, so the exact compare costs nothing.
        if current != snap.text {
            return Err(EditError::StaleTag {
                expected: snap.tag,
                got: tag_of(&current),
            });
        }
        // Validate every op's anchors before applying any.
        for op in ops {
            if op.append {
                continue;
            }
            if op.start == 0 || op.end == 0 {
                return Err(EditError::Malformed("line numbers are 1-based".to_string()));
            }
            if op.start > op.end {
                return Err(EditError::BadRange {
                    s: op.start,
                    e: op.end,
                });
            }
            let len = snap.text.lines().count();
            if op.end > len {
                return Err(EditError::OutOfRange {
                    line: op.end,
                    len,
                });
            }
            // W4: every line the op touches must have been seen, not
            // just the first. `CUT 1.=5` with only line 1 read deleted
            // four lines the model never saw — defeating the module's
            // core safety promise.
            for line_no in op.start..=op.end {
                if !snap.seen.get(line_no).copied().unwrap_or(false) {
                    return Err(EditError::UnseenAnchor { line: line_no });
                }
            }
        }
        // W3: reject overlapping non-append ops. The descending splice
        // order only composes for disjoint ranges; two ops sharing a
        // line splice against each other's shifted indices and drop or
        // duplicate content silently.
        let mut ranges: Vec<(usize, usize)> = ops
            .iter()
            .filter(|o| !o.append)
            .map(|o| (o.start, o.end))
            .collect();
        ranges.sort_unstable();
        for w in ranges.windows(2) {
            if w[1].0 <= w[0].1 {
                return Err(EditError::Overlap {
                    first: w[0],
                    second: w[1],
                });
            }
        }
        // Stage against the CURRENT text (== snapshot, just verified).
        let new_text = stage(&current, ops)?;
        // Atomic write + walk-cache invalidation: a torn write fed the
        // model garbage, and a stale listing survived 5s otherwise.
        crate::tools::atomic_write(path, new_text.as_bytes())
            .map_err(|e| EditError::Malformed(e.to_string()))?;
        crate::walk_cache::invalidate_all();
        let new_tag = tag_of(&new_text);
        // The old snapshot is now stale; drop it so a chained edit
        // must carry the new tag.
        self.snapshots.remove(&key);
        Ok((new_tag, new_text))
    }
}

/// Apply `ops` to `text`, returning the new content. Assumes the ops
/// are already validated.
fn stage(text: &str, ops: &[Op]) -> Result<String, EditError> {
    // Detect the dominant EOL before `lines()` strips `\r`, or editing
    // one line of a CRLF file rewrites the whole file's line endings.
    let crlf_count = text.matches("\r\n").count();
    let lf_count = text.matches('\n').count().saturating_sub(crlf_count);
    let crlf = crlf_count > lf_count;
    // Sort ops by start, descending, so earlier line numbers stay
    // valid as later edits shift the buffer. Appends go last.
    let mut indexed: Vec<(usize, &Op)> = ops.iter().enumerate().map(|(i, o)| (i, o)).collect();
    indexed.sort_by_key(|e| std::cmp::Reverse(e.1.start));

    let mut lines: Vec<String> = text.lines().map(|s| s.to_string()).collect();
    for (_, op) in &indexed {
        if op.append {
            lines.extend(op.payload.iter().cloned());
            continue;
        }
        let s = op.start - 1;
        let e = op.end; // exclusive index
        if s > lines.len() || e > lines.len() {
            return Err(EditError::OutOfRange {
                line: e,
                len: lines.len(),
            });
        }
        lines.splice(s..e, op.payload.iter().cloned());
    }
    let eol = if crlf { "\r\n" } else { "\n" };
    let mut out = lines.join(eol);
    // Preserve a trailing newline if the original had one.
    if text.ends_with('\n') && !out.ends_with('\n') {
        out.push_str(eol);
    }
    Ok(out)
}

/// Parse the hashline edit format from `text`: a `[path#tag]` header
/// then `PUT`/`CUT` ops. Returns `(path, tag, ops)`.
pub fn parse(text: &str) -> Result<(String, Tag, Vec<Op>), EditError> {
    let mut path: Option<String> = None;
    let mut tag: Option<Tag> = None;
    let mut ops: Vec<Op> = Vec::new();
    let mut pending: Option<Op> = None;

    let flush = |p: &mut Option<Op>, ops: &mut Vec<Op>| {
        if let Some(op) = p.take() {
            ops.push(op);
        }
    };

    for raw in text.lines() {
        let line = raw.trim_end();
        if line.starts_with('[') && line.ends_with(']') {
            // W5: a second header is a malformed edit. Pre-fix it fell
            // through to the junk-ignore branch and every op after it
            // was applied to the FIRST file — `[b.txt#..]`'s edit
            // spliced into `a.txt`.
            if path.is_some() {
                return Err(EditError::Malformed(
                    "second [path#tag] header — one edit block per call".to_string(),
                ));
            }
            let inner = &line[1..line.len() - 1];
            let (p, t) = inner
                .rsplit_once('#')
                .ok_or_else(|| EditError::Malformed("header needs [path#tag]".to_string()))?;
            let tag_bytes = u16::from_str_radix(t, 16)
                .map_err(|_| EditError::Malformed(format!("bad tag {t:?}")))?;
            path = Some(p.to_string());
            tag = Some(tag_bytes.to_be_bytes());
            continue;
        }
        if let Some(rest) = line.strip_prefix("PUT ") {
            flush(&mut pending, &mut ops);
            let rest = rest.trim_end_matches(':');
            if rest == ">$" {
                pending = Some(Op {
                    start: 0,
                    end: 0,
                    payload: Vec::new(),
                    append: true,
                });
                continue;
            }
            let (s, e) = parse_range(rest)?;
            pending = Some(Op {
                start: s,
                end: e,
                payload: Vec::new(),
                append: false,
            });
            continue;
        }
        if let Some(rest) = line.strip_prefix("CUT ") {
            flush(&mut pending, &mut ops);
            let (s, e) = parse_range(rest)?;
            ops.push(Op {
                start: s,
                end: e,
                payload: Vec::new(),
                append: false,
            });
            continue;
        }
        if let Some(payload) = line.strip_prefix('+') {
            let Some(op) = pending.as_mut() else {
                return Err(EditError::Malformed("payload without a PUT".to_string()));
            };
            op.payload.push(payload.to_string());
            continue;
        }
        // A blank line or anything else between ops: ignore.
    }
    flush(&mut pending, &mut ops);

    let path = path.ok_or_else(|| EditError::Malformed("no [path#tag] header".to_string()))?;
    let tag = tag.ok_or_else(|| EditError::Malformed("no tag in header".to_string()))?;
    Ok((path, tag, ops))
}

/// Parse `N.=M` or `N` (single line) into `(s, e)`.
fn parse_range(s: &str) -> Result<(usize, usize), EditError> {
    let s = s.trim();
    if let Some((a, b)) = s.split_once(".=") {
        let start = a
            .trim()
            .parse::<usize>()
            .map_err(|_| EditError::Malformed(format!("bad start {a:?}")))?;
        let end = b
            .trim()
            .parse::<usize>()
            .map_err(|_| EditError::Malformed(format!("bad end {b:?}")))?;
        Ok((start, end))
    } else {
        let n = s
            .parse::<usize>()
            .map_err(|_| EditError::Malformed(format!("bad line {s:?}")))?;
        Ok((n, n))
    }
}

/// Render `text` as `N:line` rows, the form a model edits against.
/// Caps at `max_lines`, appending an elision footer that teaches the
/// read selector when the file is longer.
pub fn render_numbered(text: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let shown = lines.len().min(max_lines);
    let mut out = String::new();
    for (i, l) in lines.iter().take(shown).enumerate() {
        out.push_str(&format!("{}:{}\n", i + 1, l));
    }
    if lines.len() > shown {
        out.push_str(&format!(
            "[…{} lines elided; re-read the range you need]\n",
            lines.len() - shown,
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seen_all(n: usize) -> Vec<bool> {
        vec![true; n + 1]
    }

    #[test]
    fn overlapping_ops_are_rejected() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("a.txt");
        std::fs::write(&f, "a\nb\nc\nd\n").unwrap();
        let mut store = EditStore::new();
        let tag = store.record_snapshot(&f, "a\nb\nc\nd\n", seen_all(4));
        let ops = vec![
            Op { start: 2, end: 3, payload: vec!["B".into()], append: false },
            Op { start: 3, end: 4, payload: vec!["C".into()], append: false },
        ];
        let err = store.apply(&f, tag, &ops).unwrap_err();
        assert!(matches!(err, EditError::Overlap { .. }), "got {err:?}");
        // File untouched.
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "a\nb\nc\nd\n");
    }

    #[test]
    fn two_puts_on_the_same_line_are_rejected_as_overlapping() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("a.txt");
        std::fs::write(&f, "a\nb\nc\n").unwrap();
        let mut store = EditStore::new();
        let tag = store.record_snapshot(&f, "a\nb\nc\n", seen_all(3));
        let ops = vec![
            Op { start: 2, end: 2, payload: vec!["FIRST".into()], append: false },
            Op { start: 2, end: 2, payload: vec!["SECOND".into()], append: false },
        ];
        assert!(matches!(
            store.apply(&f, tag, &ops).unwrap_err(),
            EditError::Overlap { .. }
        ));
    }

    #[test]
    fn a_cut_whose_end_lines_were_never_seen_is_rejected() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("a.txt");
        std::fs::write(&f, "a\nb\nc\nd\ne\n").unwrap();
        let mut store = EditStore::new();
        // Only line 1 seen.
        let mut seen = vec![false; 6];
        seen[1] = true;
        let tag = store.record_snapshot(&f, "a\nb\nc\nd\ne\n", seen);
        let ops = vec![Op { start: 1, end: 5, payload: vec![], append: false }];
        let err = store.apply(&f, tag, &ops).unwrap_err();
        assert!(matches!(err, EditError::UnseenAnchor { .. }), "got {err:?}");
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "a\nb\nc\nd\ne\n");
    }

    #[test]
    fn an_externally_changed_file_is_rejected() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("a.txt");
        std::fs::write(&f, "a\nb\n").unwrap();
        let mut store = EditStore::new();
        let tag = store.record_snapshot(&f, "a\nb\n", seen_all(2));
        // External edit since the read.
        std::fs::write(&f, "a\nCHANGED\n").unwrap();
        let ops = vec![Op { start: 2, end: 2, payload: vec!["X".into()], append: false }];
        let err = store.apply(&f, tag, &ops).unwrap_err();
        assert!(matches!(err, EditError::StaleTag { .. }), "got {err:?}");
    }

    #[test]
    fn a_second_header_is_a_malformed_edit() {
        let text = "[a.txt#00ff]\nPUT 1.=1:\n+for a\n[b.txt#00ff]\nPUT 9.=9:\n+for b\n";
        let err = parse(text).unwrap_err();
        assert!(matches!(err, EditError::Malformed(_)), "got {err:?}");
        assert!(err.to_string().contains("second"), "got {err}");
    }

    #[test]
    fn tag_is_stable_and_content_sensitive() {
        assert_eq!(tag_of("hello"), tag_of("hello"));
        assert_ne!(tag_of("hello"), tag_of("hello "));
        assert_ne!(tag_of("abc"), tag_of("abd"));
    }

    #[test]
    fn parse_reads_a_header_and_ops() {
        let text = "[src/main.rs#a1b2]\nPUT 2.=2:\n+new\nCUT 5.=6\n";
        let (path, tag, ops) = parse(text).expect("parses");
        assert_eq!(path, "src/main.rs");
        assert_eq!(tag, 0xa1b2u16.to_be_bytes());
        assert_eq!(ops.len(), 2);
        assert_eq!(ops[0].start, 2);
        assert_eq!(ops[0].payload, vec!["new".to_string()]);
        assert_eq!(ops[1].start, 5);
        assert_eq!(ops[1].end, 6);
    }

    #[test]
    fn apply_replaces_a_line() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("a.txt");
        std::fs::write(&f, "one\ntwo\nthree\n").unwrap();
        let mut store = EditStore::new();
        let tag = store.record_snapshot(&f, "one\ntwo\nthree\n", seen_all(3));
        let ops = vec![Op {
            start: 2,
            end: 2,
            payload: vec!["TWO".to_string()],
            append: false,
        }];
        let (new_tag, new_text) = store.apply(&f, tag, &ops).expect("applies");
        assert_eq!(new_text, "one\nTWO\nthree\n");
        assert_ne!(new_tag, tag);
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "one\nTWO\nthree\n");
    }

    #[test]
    fn a_stale_tag_is_rejected() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("a.txt");
        std::fs::write(&f, "one\ntwo\n").unwrap();
        let mut store = EditStore::new();
        let tag = store.record_snapshot(&f, "one\ntwo\n", seen_all(2));
        // The file changes underneath.
        std::fs::write(&f, "one\nCHANGED\n").unwrap();
        let wrong = [0xff, 0xff];
        let err = store
            .apply(&f, wrong, &[Op { start: 2, end: 2, payload: vec!["x".into()], append: false }])
            .unwrap_err();
        assert!(matches!(err, EditError::StaleTag { .. }), "got {err:?}");
        // The file is untouched.
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "one\nCHANGED\n");
        let _ = tag;
    }

    #[test]
    fn an_unseen_anchor_is_rejected() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("a.txt");
        std::fs::write(&f, "one\ntwo\nthree\n").unwrap();
        let mut store = EditStore::new();
        // Only line 1 was seen.
        let mut seen = vec![false; 4];
        seen[1] = true;
        let tag = store.record_snapshot(&f, "one\ntwo\nthree\n", seen);
        let err = store
            .apply(&f, tag, &[Op { start: 3, end: 3, payload: vec!["x".into()], append: false }])
            .unwrap_err();
        assert!(matches!(err, EditError::UnseenAnchor { line: 3 }), "got {err:?}");
        // Untouched.
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "one\ntwo\nthree\n");
    }

    #[test]
    fn apply_is_all_or_nothing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("a.txt");
        std::fs::write(&f, "one\ntwo\n").unwrap();
        let mut store = EditStore::new();
        let tag = store.record_snapshot(&f, "one\ntwo\n", seen_all(2));
        // First op is valid, second addresses an unseen line.
        let mut seen = vec![false; 3];
        seen[1] = true;
        store.record_snapshot(&f, "one\ntwo\n", seen);
        let tag2 = store.current_tag(&f).unwrap();
        let _ = tag;
        let ops = vec![
            Op { start: 1, end: 1, payload: vec!["ONE".into()], append: false },
            Op { start: 2, end: 2, payload: vec!["TWO".into()], append: false },
        ];
        assert!(store.apply(&f, tag2, &ops).is_err());
        // No partial write.
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "one\ntwo\n");
    }

    #[test]
    fn append_adds_at_the_end() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("a.txt");
        std::fs::write(&f, "one\n").unwrap();
        let mut store = EditStore::new();
        let tag = store.record_snapshot(&f, "one\n", seen_all(1));
        let ops = vec![Op {
            start: 0,
            end: 0,
            payload: vec!["two".into(), "three".into()],
            append: true,
        }];
        let (_, text) = store.apply(&f, tag, &ops).unwrap();
        assert_eq!(text, "one\ntwo\nthree\n");
    }

    #[test]
    fn an_edit_without_a_snapshot_is_rejected() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("a.txt");
        std::fs::write(&f, "x\n").unwrap();
        let mut store = EditStore::new();
        let err = store
            .apply(&f, [0, 0], &[Op { start: 1, end: 1, payload: vec![], append: false }])
            .unwrap_err();
        assert_eq!(err, EditError::NeverRead);
    }

    #[test]
    fn render_numbered_prefixes_lines() {
        let out = render_numbered("alpha\nbeta\n", 10);
        assert_eq!(out, "1:alpha\n2:beta\n");
    }

    #[test]
    fn render_numbered_elides_past_the_cap() {
        let text = "a\nb\nc\nd\n";
        let out = render_numbered(text, 2);
        assert!(out.starts_with("1:a\n2:b\n"));
        assert!(out.contains("2 lines elided"), "got: {out}");
    }

    #[test]
    fn a_stale_edit_leaves_the_snapshot_for_a_retry() {
        // After a *rejected* edit the snapshot is still there, so a
        // corrected edit with the right tag succeeds without a
        // re-read.
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("a.txt");
        std::fs::write(&f, "one\ntwo\n").unwrap();
        let mut store = EditStore::new();
        let tag = store.record_snapshot(&f, "one\ntwo\n", seen_all(2));
        // Wrong tag first.
        let _ = store.apply(&f, [0, 0], &[]);
        // Right tag still works.
        let (_, text) = store
            .apply(&f, tag, &[Op { start: 1, end: 1, payload: vec!["ONE".into()], append: false }])
            .unwrap();
        assert_eq!(text, "ONE\ntwo\n");
    }
}
