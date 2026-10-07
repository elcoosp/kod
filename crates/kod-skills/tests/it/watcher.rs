//! Tests for the skill file watcher.
//!
//! macOS FSEvents reports an in-place rewrite (truncate + write) as
//! `Created` rather than `Modified`, and delivers events with a
//! configurable coalescing latency (about one second by default).
//! Under load — a full `cargo nextest run --workspace`, for
//! instance — the system-wide FSEvents service delays delivery
//! further, so a test that asserts on the *first* event received
//! flakes. These tests wait for the event they actually care about,
//! skipping over platform-specific noise, and give the OS a
//! generous settle window before the operation under test.

use kod_skills::{SkillWatcher, WatchEvent};
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tokio::sync::mpsc;

/// Maximum time to wait for an event before failing.
const EVENT_TIMEOUT: Duration = Duration::from_secs(15);

/// Time to wait after installing the watcher so the OS has a chance
/// to deliver any events for files that already exist, and to
/// establish the subscription itself. Two seconds covers the
/// FSEvents default latency even on a loaded CI box.
const WATCHER_SETTLE: Duration = Duration::from_millis(2_000);

/// Drain every queued event for `duration`, discarding what is
/// found. Used to clear the initial burst so the wait below sees
/// only events from the operation under test.
async fn drain_for(rx: &mut mpsc::Receiver<WatchEvent>, duration: Duration) {
    let deadline = Instant::now() + duration;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return;
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(_)) => continue,
            Ok(None) | Err(_) => return,
        }
    }
}

/// Wait for an event matching `predicate`, skipping anything else.
/// Returns `None` on timeout or channel close.
async fn wait_for<F>(rx: &mut mpsc::Receiver<WatchEvent>, mut predicate: F) -> Option<WatchEvent>
where
    F: FnMut(&WatchEvent) -> bool,
{
    let deadline = Instant::now() + EVENT_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(ev)) => {
                if predicate(&ev) {
                    return Some(ev);
                }
            }
            Ok(None) | Err(_) => return None,
        }
    }
}

#[tokio::test]
async fn test_watch_new_file() {
    let tmp = TempDir::new().unwrap();
    let skills_dir = tmp.path().join("skills");
    std::fs::create_dir_all(&skills_dir).unwrap();

    let (_watcher, mut rx) = SkillWatcher::new(&skills_dir).unwrap();
    tokio::time::sleep(WATCHER_SETTLE).await;
    drain_for(&mut rx, Duration::from_millis(300)).await;

    std::fs::write(skills_dir.join("brand_new.md"), "content\n").unwrap();

    let ev = wait_for(
        &mut rx,
        |e| matches!(e, WatchEvent::Created(p) if p.ends_with("brand_new.md")),
    )
    .await;
    assert!(
        ev.is_some(),
        "expected a Created event for brand_new.md within {EVENT_TIMEOUT:?}"
    );
}

#[tokio::test]
async fn test_hot_reload_on_file_change() {
    let tmp = TempDir::new().unwrap();
    let skills_dir = tmp.path().join("skills");
    std::fs::create_dir_all(&skills_dir).unwrap();

    // Create the file BEFORE installing the watcher. The initial
    // write is not what this test measures; the modification below
    // is. Anything the watcher reports for the pre-existing file is
    // drained away.
    let path = skills_dir.join("test.md");
    std::fs::write(&path, "before\n").unwrap();

    let (_watcher, mut rx) = SkillWatcher::new(&skills_dir).unwrap();
    tokio::time::sleep(WATCHER_SETTLE).await;
    drain_for(&mut rx, Duration::from_millis(300)).await;

    std::fs::write(&path, "after\n").unwrap();

    // Either Created or Modified is a valid "the file changed"
    // signal: macOS FSEvents reports an in-place rewrite as Created
    // (the truncate is treated as a fresh file), Linux inotify
    // reports Modified. The consumer's reload path handles both.
    let ev = wait_for(&mut rx, |e| match e {
        WatchEvent::Created(p) | WatchEvent::Modified(p) => p.ends_with("test.md"),
        _ => false,
    })
    .await;
    assert!(
        ev.is_some(),
        "expected a change event for test.md within {EVENT_TIMEOUT:?}"
    );
}

#[tokio::test]
async fn test_watch_file_deletion() {
    let tmp = TempDir::new().unwrap();
    let skills_dir = tmp.path().join("skills");
    std::fs::create_dir_all(&skills_dir).unwrap();

    let path = skills_dir.join("to_delete.md");
    std::fs::write(&path, "content\n").unwrap();

    let (_watcher, mut rx) = SkillWatcher::new(&skills_dir).unwrap();
    tokio::time::sleep(WATCHER_SETTLE).await;
    drain_for(&mut rx, Duration::from_millis(300)).await;

    std::fs::remove_file(&path).unwrap();

    let ev = wait_for(
        &mut rx,
        |e| matches!(e, WatchEvent::Removed(p) if p.ends_with("to_delete.md")),
    )
    .await;
    assert!(
        ev.is_some(),
        "expected a Removed event for to_delete.md within {EVENT_TIMEOUT:?}"
    );
}
