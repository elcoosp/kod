//! On-disk persistence for per-transcript engine state (Tier 3.4
//! follow-up).
//!
//! The plan and the decision log are `RwLock<HashMap>` on the engine.
//! They are cheap to hold in memory but they are *durable* by intent —
//! a decision the user made on turn 3 is still relevant on turn 30.
//! This module reads and writes the whole map to `state.json` next to
//! the trace log, so a restart resumes with the same plans and the
//! same decisions.
//!
//! # Concurrency
//!
//! The writer is synchronous and uses the same
//! write-temp-then-rename discipline as `KodApp::save_session`: two
//! kod processes on the same directory do not corrupt each other's
//! file, and a crash mid-write leaves the previous state intact.
//!
//! # Failure mode
//!
//! Every operation is best-effort. A missing file, a corrupt file, a
//! permission error — all are logged at `debug`/`warn` and treated as
//! "no prior state". Losing a persisted plan is not worth failing a
//! session over.

use crate::decisions::DecisionLog;
use crate::plan::Plan;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// The whole persisted state, keyed by transcript.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct EngineState {
    /// Current schema version. Bumped on breaking changes; a file
    /// with a higher version is loaded best-effort and a warning is
    /// logged.
    pub schema_version: u32,
    pub plans: HashMap<String, Plan>,
    pub decision_logs: HashMap<String, DecisionLog>,
}

/// Current schema version. Bump on breaking changes; additive fields
/// do not require a bump because `#[serde(default)]` handles the
/// missing-key case.
pub const STATE_SCHEMA_VERSION: u32 = 1;

/// The on-disk persistence handle. Clone shares the path; the file
/// itself is written synchronously on every call.
#[derive(Debug, Clone)]
pub struct StateStore {
    path: PathBuf,
}

impl StateStore {
    /// Open (or create the parent for) the state file at `path`.
    pub fn open(path: PathBuf) -> Self {
        Self { path }
    }

    /// Default location for a session: `state.json` next to the trace
    /// log, which sits in `~/.kod/sessions/`.
    pub fn default_path() -> Option<PathBuf> {
        dirs::home_dir().map(|h| {
            h.join(".kod").join("sessions").join("state.json")
        })
    }

    /// Sibling of a `turns.jsonl` file: same directory, `state.json`.
    pub fn sibling_of(trace_path: &Path) -> Option<PathBuf> {
        trace_path.parent().map(|p| p.join("state.json"))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Load the state from disk. Returns an empty state on any error,
    /// logging the reason. A file that is missing is not an error —
    /// it is the normal first-run case.
    pub fn load(&self) -> EngineState {
        match std::fs::read_to_string(&self.path) {
            Ok(raw) => match serde_json::from_str::<EngineState>(&raw) {
                Ok(mut state) => {
                    if state.schema_version > STATE_SCHEMA_VERSION {
                        tracing::warn!(
                            found = state.schema_version,
                            expected = STATE_SCHEMA_VERSION,
                            path = %self.path.display(),
                            "state.json was written by a newer kod; loading best-effort",
                        );
                    }
                    state.schema_version = STATE_SCHEMA_VERSION;
                    state
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        path = %self.path.display(),
                        "state.json is unreadable; starting fresh",
                    );
                    EngineState::default()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                EngineState::default()
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    path = %self.path.display(),
                    "could not read state.json; starting fresh",
                );
                EngineState::default()
            }
        }
    }

    /// Write the state atomically: temp file + rename. A crash mid-write
    /// leaves the previous state intact.
    pub fn save(&self, state: &EngineState) -> Result<(), std::io::Error> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut to_write = state.clone();
        to_write.schema_version = STATE_SCHEMA_VERSION;
        let raw = serde_json::to_string_pretty(&to_write)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let tmp = self.path.with_extension(format!(
            "tmp.{}.{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        ));
        std::fs::write(&tmp, raw.as_bytes())?;
        if let Err(e) = std::fs::rename(&tmp, &self.path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decisions::{DecisionAuthor, DecisionKind, DecisionLog};
    use crate::plan::Plan;
    use tempfile::TempDir;

    #[test]
    fn load_missing_file_is_empty() {
        let tmp = TempDir::new().unwrap();
        let store = StateStore::open(tmp.path().join("state.json"));
        let s = store.load();
        assert!(s.plans.is_empty());
        assert!(s.decision_logs.is_empty());
    }

    #[test]
    fn save_then_load_roundtrips() {
        let tmp = TempDir::new().unwrap();
        let store = StateStore::open(tmp.path().join("state.json"));
        let mut state = EngineState::default();
        state
            .plans
            .insert("session".into(), Plan::new("goal", vec!["step".into()]));
        let mut log = DecisionLog::new();
        log.push(
            1,
            DecisionKind::UserPreference,
            "prefer tabs".into(),
            DecisionAuthor::User,
        );
        state.decision_logs.insert("session".into(), log);
        store.save(&state).unwrap();

        let back = store.load();
        assert_eq!(back.plans.len(), 1);
        assert!(back.plans.contains_key("session"));
        assert_eq!(back.decision_logs.len(), 1);
        assert_eq!(
            back.decision_logs["session"].entries[0].text,
            "prefer tabs",
        );
    }

    #[test]
    fn corrupt_file_loads_empty() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("state.json");
        std::fs::write(&path, "{ not valid").unwrap();
        let store = StateStore::open(path);
        let s = store.load();
        assert!(s.plans.is_empty());
    }

    #[test]
    fn save_creates_parent_dirs() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("a").join("b").join("state.json");
        let store = StateStore::open(path.clone());
        store.save(&EngineState::default()).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn save_is_atomic_and_cleans_temp() {
        let tmp = TempDir::new().unwrap();
        let store = StateStore::open(tmp.path().join("state.json"));
        store.save(&EngineState::default()).unwrap();
        // No leftover temp files in the directory.
        let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path()
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.contains("tmp"))
                    .unwrap_or(false)
            })
            .collect();
        assert!(leftovers.is_empty(), "found temp files: {leftovers:?}");
    }

    #[test]
    fn higher_schema_version_loads_best_effort() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("state.json");
        std::fs::write(
            &path,
            r#"{"schema_version":99,"plans":{},"decision_logs":{}}"#,
        )
        .unwrap();
        let store = StateStore::open(path);
        let s = store.load();
        // The version is reset to the current one, but the load
        // succeeded.
        assert_eq!(s.schema_version, STATE_SCHEMA_VERSION);
    }

    #[test]
    fn sibling_of_trace_path() {
        let p = PathBuf::from("/home/u/.kod/sessions/turns.jsonl");
        let s = StateStore::sibling_of(&p).unwrap();
        assert_eq!(s, PathBuf::from("/home/u/.kod/sessions/state.json"));
    }
}
