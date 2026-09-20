//! File system watcher for hot reloading skills.
//!
//! Uses the notify crate to watch for changes to skill files
//! and emits events when files are created, modified, or removed.

use kod_error::{KodError, Result};
use notify::{Event as NotifyEvent, RecommendedWatcher, RecursiveMode, Watcher as NotifyWatcher};
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;

/// Events emitted by the skill watcher
#[derive(Debug, Clone, PartialEq)]
pub enum WatchEvent {
    Created(PathBuf),
    Modified(PathBuf),
    Removed(PathBuf),
}

/// Watches a directory for skill file changes
pub struct SkillWatcher {
    watcher: RecommendedWatcher,
    watch_dir: PathBuf,
    is_running: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl SkillWatcher {
    /// Create a new watcher for the given directory
    pub fn new(watch_dir: &Path) -> Result<(Self, mpsc::Receiver<WatchEvent>)> {
        // One bounded tokio channel carries events from the notify
        // callback straight to the returned receiver. The previous
        // design had two channels and a polling task in between: a
        // `std_mpsc` channel fed by the callback, polled by a
        // `tokio::spawn`ed task that woke every 100 ms and forwarded
        // the event through `tokio::sync::mpsc::Sender::blocking_send`.
        // That design had three defects, all of which the un-ignored
        // tests surfaced:
        //
        //   1. `std::thread::sleep` inside a `tokio::spawn`ed task
        //      blocks a tokio worker thread. On a `#[tokio::test]`
        //      (current-thread runtime, one worker) it blocks the
        //      *only* worker, so the test hung past the CI timeout.
        //   2. `blocking_send` inside an async task is documented
        //      as "may block the executor thread"; combined with the
        //      100 ms sleep the polling loop consumed one worker.
        //   3. `is_running.load()` was read at the top of the poll
        //      loop, before the caller had a chance to call
        //      `start()`. Under a runtime that scheduled the spawned
        //      task before `start()` ran, the loop exited immediately
        //      and no events were ever forwarded.
        //
        // The fix is a direct hand-off: the notify callback fires on
        // notify's own thread (not a tokio worker), and calls
        // `try_send` on the tokio sender — a non-blocking operation
        // that is legal outside an async context. The consumer side
        // is a plain `recv().await`. No intermediate channel, no
        // polling loop, no worker thread held.
        //
        // # Canonical vs caller spelling
        //
        // On macOS, `recommended_watcher` (FSEvents) reports paths in
        // *canonical* form: `/private/var/folders/...`. In the same
        // process, `tempfile::TempDir` and `std::env::temp_dir()`
        // return `/var/folders/...`, which is a symlink into
        // `/private/var`. A naive `path.starts_with(&watch_dir)`
        // check therefore rejects every event under a tempdir, which
        // is exactly what the diagnostic showed: the raw notify
        // stream fired three events, all dropped by the prefix check.
        //
        // We canonicalize `watch_dir` once for the prefix match, and
        // then re-express the incoming path *back in the caller's
        // spelling* before sending it on. That gives the caller a path
        // they can compare directly against one they built — the
        // property the watcher tests assert on.
        let (tx, rx) = mpsc::channel(256);
        let caller_dir = watch_dir.to_path_buf();
        let canon_dir = std::fs::canonicalize(watch_dir).unwrap_or_else(|_| caller_dir.clone());

        let mut watcher = notify::recommended_watcher(
            move |res: std::result::Result<NotifyEvent, notify::Error>| {
                let Ok(event) = res else { return };
                for path in event.paths {
                    // Only `.md` files: the skill watcher is not a
                    // general-purpose file watcher.
                    if path.extension().and_then(|s| s.to_str()) != Some("md") {
                        continue;
                    }
                    // Match in canonical space so an event whose path
                    // is spelled `/private/var/...` matches a
                    // `watch_dir` spelled `/var/...`. A path that
                    // escapes the watched tree is not our event.
                    let Ok(rel) = path.strip_prefix(&canon_dir) else {
                        continue;
                    };
                    // Re-express under the caller's spelling.
                    let caller_path = caller_dir.join(rel);
                    let watch_event = match event.kind {
                        notify::EventKind::Create(_) => WatchEvent::Created(caller_path),
                        notify::EventKind::Modify(_) => WatchEvent::Modified(caller_path),
                        notify::EventKind::Remove(_) => WatchEvent::Removed(caller_path),
                        _ => continue,
                    };
                    // `try_send` on a bounded channel: a full queue
                    // drops the event rather than blocking the notify
                    // thread. Capacity 256 is far above the burst an
                    // editor produces on a save.
                    let _ = tx.try_send(watch_event);
                }
            },
        )
        .map_err(|e| KodError::Internal(format!("Failed to create watcher: {}", e)))?;

        watcher
            .watch(watch_dir, RecursiveMode::Recursive)
            .map_err(|e| KodError::Internal(format!("Failed to watch directory: {}", e)))?;

        Ok((
            Self {
                watcher,
                watch_dir: watch_dir.to_path_buf(),
                is_running: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            },
            rx,
        ))
    }

    /// Start the watcher (it's already watching, this is for state tracking)
    pub fn start(&self) -> Result<()> {
        self.is_running
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    /// Stop the watcher
    pub fn stop(&mut self) -> Result<()> {
        self.is_running
            .store(false, std::sync::atomic::Ordering::SeqCst);
        // Unwatch to stop receiving events
        let _ = NotifyWatcher::unwatch(&mut self.watcher, &self.watch_dir);
        Ok(())
    }

    /// Check if watcher is running
    pub fn is_running(&self) -> bool {
        self.is_running.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Get the watched directory
    pub fn watch_dir(&self) -> &Path {
        &self.watch_dir
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_watcher_creation() {
        let temp_dir = TempDir::new().unwrap();
        let skills_dir = temp_dir.path().join("skills");
        std::fs::create_dir_all(&skills_dir).unwrap();

        let (watcher, _rx) = SkillWatcher::new(&skills_dir).unwrap();
        assert_eq!(watcher.watch_dir(), skills_dir);
    }
}

#[cfg(test)]
mod coverage_watch_event {
    //! `WatchEvent` is `PartialEq`, and a caller that branches on
    //! the variant needs equality to behave. The variant's inner
    //! path is part of the identity — two Created events on
    //! different paths are not equal, and that is what makes a
    //! filter like `matches!(ev, WatchEvent::Created(p) if
    //! p.ends_with("x"))` work.
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn events_with_the_same_variant_and_path_are_equal() {
        let a = WatchEvent::Created(PathBuf::from("/a/b"));
        let b = WatchEvent::Created(PathBuf::from("/a/b"));
        assert_eq!(a, b);
    }

    #[test]
    fn events_with_the_same_variant_but_different_paths_are_not_equal() {
        let a = WatchEvent::Created(PathBuf::from("/a/b"));
        let c = WatchEvent::Created(PathBuf::from("/a/c"));
        assert_ne!(a, c);
    }

    #[test]
    fn events_with_different_variants_are_not_equal() {
        let p = PathBuf::from("/a/b");
        assert_ne!(
            WatchEvent::Created(p.clone()),
            WatchEvent::Modified(p.clone())
        );
        assert_ne!(
            WatchEvent::Created(p.clone()),
            WatchEvent::Removed(p.clone())
        );
        assert_ne!(WatchEvent::Modified(p.clone()), WatchEvent::Removed(p));
    }

    #[test]
    fn events_clone_preserves_identity() {
        let e = WatchEvent::Modified(PathBuf::from("/x/y"));
        let c = e.clone();
        assert_eq!(e, c);
    }

    #[test]
    fn debug_output_names_the_variant() {
        let e = WatchEvent::Created(PathBuf::from("/a"));
        let s = format!("{e:?}");
        assert!(s.contains("Created"), "got: {s}");
        assert!(s.contains("/a"), "got: {s}");
    }
}
