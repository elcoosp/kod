//! File system watcher for hot reloading skills.
//!
//! Uses the notify crate to watch for changes to skill files
//! and emits events when files are created, modified, or removed.

use kod_error::{KodError, Result};
use notify::{Event as NotifyEvent, RecommendedWatcher, RecursiveMode, Watcher as NotifyWatcher};
use std::path::{Path, PathBuf};
use std::sync::mpsc as std_mpsc;
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
        let (event_tx, event_rx) = mpsc::channel(100);
        let (notify_tx, notify_rx) = std_mpsc::channel();

        let mut watcher = notify::recommended_watcher(move |res: std::result::Result<NotifyEvent, notify::Error>| {
                if let Ok(event) = res {
                    let _ = notify_tx.send(event);
                }
            })
            .map_err(|e| KodError::Internal(format!("Failed to create watcher: {}", e)))?;

        watcher
            .watch(watch_dir, RecursiveMode::Recursive)
            .map_err(|e| KodError::Internal(format!("Failed to watch directory: {}", e)))?;

        let watcher_handle = Self {
            watcher,
            watch_dir: watch_dir.to_path_buf(),
            is_running: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };

        // Spawn task to process events
        let is_running = watcher_handle.is_running.clone();
        let watch_dir_clone = watch_dir.to_path_buf();

        tokio::spawn(async move {
            process_notify_events(notify_rx, event_tx, watch_dir_clone, is_running);
        });

        Ok((watcher_handle, event_rx))
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
        self.is_running
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Get the watched directory
    pub fn watch_dir(&self) -> &Path {
        &self.watch_dir
    }
}

/// Process raw notify events and convert to our WatchEvent format
fn process_notify_events(
    receiver: std_mpsc::Receiver<NotifyEvent>,
    sender: mpsc::Sender<WatchEvent>,
    watch_dir: PathBuf,
    is_running: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    while is_running.load(std::sync::atomic::Ordering::SeqCst) {
        // Non-blocking check for events
        match receiver.try_recv() {
            Ok(event) => {
                for path in event.paths {
                    // Only process .md files
                    if path.extension().and_then(|s| s.to_str()) != Some("md") {
                        continue;
                    }

                    // Only process paths within our watch directory
                    if !path.starts_with(&watch_dir) {
                        continue;
                    }

                    // Convert notify event kind to our WatchEvent
                    let watch_event = match event.kind {
                        notify::EventKind::Create(_) => WatchEvent::Created(path),
                        notify::EventKind::Modify(_) => WatchEvent::Modified(path),
                        notify::EventKind::Remove(_) => WatchEvent::Removed(path),
                        _ => continue,
                    };

                    // Try to send (ignore if receiver dropped)
                    let _ = sender.blocking_send(watch_event);
                }
            }
            Err(std_mpsc::TryRecvError::Empty) => {
                // No events, sleep briefly
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(std_mpsc::TryRecvError::Disconnected) => {
                // Channel closed, exit
                break;
            }
        }
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
