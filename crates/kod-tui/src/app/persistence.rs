//! Prompt history and session persistence on `KodApp`.
//!
//! Extracted from `app/mod.rs` (S9). Two files: `tui_history.json`
//! (the up/down ring) and `tui_session.json` (the transcript, with a
//! `schema_version` and a legacy bare-array fallback).

use super::*;

impl KodApp {
    pub(super) fn state_dir() -> Option<std::path::PathBuf> {
        if let Ok(dir) = std::env::var("KOD_TUI_STATE_DIR") {
            return Some(std::path::PathBuf::from(dir));
        }
        dirs::home_dir().map(|h| h.join(".kod"))
    }

    pub fn history_path() -> Option<std::path::PathBuf> {
        Self::state_dir().map(|d| d.join("tui_history.json"))
    }

    pub fn session_path() -> Option<std::path::PathBuf> {
        Self::state_dir().map(|d| d.join("tui_session.json"))
    }

    /// Load persisted prompt history (startup). Best-effort: missing or
    /// corrupt files just mean a fresh history.
    pub fn load_persistent_history(&mut self) {
        let Some(path) = Self::history_path() else {
            return;
        };
        let Ok(raw) = std::fs::read_to_string(&path) else {
            return;
        };
        if let Ok(entries) = serde_json::from_str::<Vec<String>>(&raw) {
            for e in entries.into_iter().take(500) {
                if !e.trim().is_empty() && !self.input_history.contains(&e) {
                    self.input_history.push(e);
                }
            }
            if self.input_history.len() > 500 {
                let drop = self.input_history.len() - 500;
                self.input_history.drain(..drop);
            }
        }
    }

    /// Append one entry to the history file (called after submit).
    ///
    /// Two TUI sessions running concurrently (one in each of two
    /// worktrees, say) read and write the same
    /// `~/.kod/tui_history.json`. The previous unlocked
    /// read-modify-write could interleave (A reads, B writes, A
    /// writes) and silently discard everything B appended.
    ///
    /// Rather than take an advisory lock — the `fs4` crate's module
    /// path depends on the feature set and version, and this crate
    /// does not otherwise need it — write to a sibling temp file and
    /// rename over the target. `rename` is atomic on POSIX and on
    /// Windows (via ReplaceFile semantics under the std
    /// implementation), so no reader ever sees a partially written
    /// file. The worst case is one session's last append losing to
    /// the other's — the same last-writer-wins as before, without
    /// the risk of a truncated read.
    ///
    /// Best-effort throughout: history is a convenience, never a
    /// correctness requirement, and a failed save must not surface as
    /// an error.
    pub fn persist_history_entry(&mut self, entry: &str) {
        let entry = entry.trim();
        if entry.is_empty() {
            return;
        }
        // Keep the in-memory list in sync when dispatch bypassed submit.
        if self.input_history.last().map(|s| s.as_str()) != Some(entry) {
            self.input_history.push(entry.to_string());
        }
        let Some(path) = Self::history_path() else {
            return;
        };
        let Some(parent) = path.parent() else {
            return;
        };
        let _ = std::fs::create_dir_all(parent);

        // Merge with whatever is on disk.
        let mut entries: Vec<String> = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default();
        if entries.last().map(|s| s.as_str()) != Some(entry) {
            entries.push(entry.to_string());
        }
        if entries.len() > 500 {
            let drop = entries.len() - 500;
            entries.drain(..drop);
        }

        // Write to a unique sibling, then rename over the target.
        // The pid+suffix keeps two sessions from colliding on the
        // temp file itself.
        let tmp = parent.join(format!(
            "tui_history.json.tmp.{}.{}",
            std::process::id(),
            // Nanoseconds since the epoch, cheap unique-ish suffix
            // without pulling in a random-number crate.
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        ));
        let serialized = match serde_json::to_string(&entries) {
            Ok(s) => s,
            Err(_) => return,
        };
        if std::fs::write(&tmp, serialized.as_bytes()).is_err() {
            let _ = std::fs::remove_file(&tmp);
            return;
        }
        if std::fs::rename(&tmp, &path).is_err() {
            // Rename failed (cross-device? permissions?). Clean up the
            // temp so we do not accumulate orphans, and give up.
            let _ = std::fs::remove_file(&tmp);
        }
    }

    /// Save the current chat for session restore (called on quit /
    /// after each assistant reply — cheap enough at chat scale).
    ///
    /// Writes to a sibling temp file then renames over the target.
    /// `std::fs::write` truncates the destination before writing, so a
    /// crash between the truncate and the last byte left a zero-byte
    /// or half-written session file — which `load_session` then
    /// discards, silently losing the transcript the user was trying
    /// to save. The rename is atomic on POSIX and Windows, so a
    /// reader either sees the complete previous file or the complete
    /// new one.
    ///
    /// Also removes the second-writer hazard the same way
    /// `persist_history_entry` does: two TUI processes shutting down
    /// concurrently can each serialize a session, but the loser's
    /// rename is the only observable outcome. No interleaved partial
    /// file.
    pub fn save_session(&self) {
        let Some(path) = Self::session_path() else {
            return;
        };
        let Some(parent) = path.parent() else {
            return;
        };
        let keep = self.messages.len().saturating_sub(200);
        let snapshot = &self.messages[keep..];
        let _ = std::fs::create_dir_all(parent);

        // Tier 3.2 — wrap the array in a `SessionSnapshot` so a
        // future schema bump is detectable. The `.messages` field is
        // byte-identical to the legacy array, so the file is not
        // substantially larger.
        let snapshot_owned = snapshot.to_vec();
        let wrapper = SessionSnapshot {
            schema_version: SESSION_SCHEMA_VERSION,
            messages: snapshot_owned,
        };
        let Ok(raw) = serde_json::to_string(&wrapper) else {
            return;
        };

        // Unique temp per process + nanosecond clock. Two processes
        // writing at once get different temps and the rename lets the
        // last one win; neither leaves a partial file behind.
        let tmp = parent.join(format!(
            "tui_session.json.tmp.{}.{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        ));
        if std::fs::write(&tmp, raw.as_bytes()).is_err() {
            let _ = std::fs::remove_file(&tmp);
            return;
        }
        if std::fs::rename(&tmp, &path).is_err() {
            // Rename failed (cross-device temp, permissions on the
            // target directory). Clean up the temp; the previous
            // session file remains untouched on disk.
            let _ = std::fs::remove_file(&tmp);
        }
    }

    /// Restore the saved chat. Returns the restored message count.
    pub fn load_session(&mut self) -> usize {
        let Some(path) = Self::session_path() else {
            return 0;
        };
        let Ok(raw) = std::fs::read_to_string(&path) else {
            return 0;
        };
        // Tier 3.2 — accept both the current wrapper shape and the
        // pre-3.2 bare-array shape. The wrapper is tried first; a
        // bare array fails its `messages` field and falls through.
        let mut msgs: Vec<Message> = match serde_json::from_str::<SessionSnapshot>(&raw) {
            Ok(snap) => {
                // A future file's schema_version we do not know: the
                // loader tolerates it (messages parse) but logs so a
                // downgrade is visible.
                if snap.schema_version > SESSION_SCHEMA_VERSION {
                    tracing::warn!(
                        found = snap.schema_version,
                        expected = SESSION_SCHEMA_VERSION,
                        "tui_session.json was written by a newer kod; loading best-effort",
                    );
                }
                snap.messages
            }
            Err(_) => match serde_json::from_str::<Vec<Message>>(&raw) {
                Ok(v) => v,
                Err(_) => return 0,
            },
        };
        let n = msgs.len();
        // Restore monotonic sequence after a restart: `next_seq` must
        // be past the highest stored sequence, otherwise new messages
        // would sort before restored ones.
        //
        // Pre-sequence session files (all `sequence == 0`) are
        // backfilled in file order below, which changes `next_seq`.
        // Files written by the current code (any non-zero sequence)
        // use the max-sequence path.
        let max_seq = msgs.iter().map(|m| m.sequence).max().unwrap_or(0);
        self.next_seq = max_seq + msgs.len() as u64 + 1;

        if msgs.iter().all(|m| m.sequence == 0) && !msgs.is_empty() {
            // Legacy file: assign in file order so the chat widget's
            // sequence sort is a no-op on this file. `next_seq` is set
            // to `len()` so the next new message lands after all of
            // them. (The previous implementation had this branch
            // preceded by an empty `for` loop that did nothing — a
            // leftover from an earlier design that never ran.)
            for (i, m) in msgs.iter_mut().enumerate() {
                m.sequence = i as u64;
            }
            self.next_seq = msgs.len() as u64;
        }

        self.messages = msgs;
        self.scroll_to_bottom();
        n
    }
}
