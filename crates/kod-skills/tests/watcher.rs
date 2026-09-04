use kod_skills::loader::SkillLoader;
use kod_skills::watcher::SkillWatcher;
use kod_skills::watcher::WatchEvent;
use std::fs;
use std::time::Duration;
use tempfile::TempDir;

async fn wait_for_event(
    events: &mut tokio::sync::mpsc::Receiver<WatchEvent>,
    timeout_secs: u64,
) -> Result<WatchEvent, String> {
    let timeout = Duration::from_secs(timeout_secs);
    match tokio::time::timeout(timeout, events.recv()).await {
        Ok(Some(event)) => Ok(event),
        Ok(None) => Err("Channel closed".to_string()),
        Err(_) => Err("Timeout waiting for event".to_string()),
    }
}

/// These tests require filesystem event notification which can be flaky
/// depending on OS and filesystem. They are ignored by default.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "filesystem event notification can be flaky"]
async fn test_hot_reload_on_file_change() {
    let temp_dir = TempDir::new().unwrap();
    let skills_dir = temp_dir.path().join("skills");
    fs::create_dir_all(&skills_dir).unwrap();

    // Create initial skill
    let skill_file = skills_dir.join("test.md");
    fs::write(
        &skill_file,
        r#"---
name: test
description: Original description
version: 1.0.0
category: test
---

## Instructions

Original.
"#,
    )
    .unwrap();

    // Set up loader and watcher
    let mut loader = SkillLoader::new(&skills_dir);
    loader.load_all().await.unwrap();

    let (mut watcher, mut events) = SkillWatcher::new(&skills_dir).unwrap();
    watcher.start().unwrap();

    // Modify the file
    tokio::time::sleep(Duration::from_millis(200)).await;
    fs::write(
        &skill_file,
        r#"---
name: test
description: Updated description
version: 1.0.1
category: test
---

## Instructions

Updated.
"#,
    )
    .unwrap();

    // Wait for watcher to detect change
    let event = wait_for_event(&mut events, 5).await.unwrap();

    match event {
        WatchEvent::Modified(path) => {
            assert_eq!(path, skill_file);
        }
        ref e => panic!("Expected Modified event, got {:?}", e),
    }

    // Cleanup
    watcher.stop().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "filesystem event notification can be flaky"]
async fn test_watch_new_file() {
    let temp_dir = TempDir::new().unwrap();
    let skills_dir = temp_dir.path().join("skills");
    fs::create_dir_all(&skills_dir).unwrap();

    let (mut watcher, mut events) = SkillWatcher::new(&skills_dir).unwrap();
    watcher.start().unwrap();

    // Create a new skill file
    tokio::time::sleep(Duration::from_millis(200)).await;
    let new_skill = skills_dir.join("new.md");
    fs::write(
        &new_skill,
        r#"---
name: new
description: New skill
version: 1.0.0
category: test
---

## Instructions

New.
"#,
    )
    .unwrap();

    // Wait for event
    let event = wait_for_event(&mut events, 5).await.unwrap();

    match event {
        WatchEvent::Created(path) => {
            assert_eq!(path, new_skill);
        }
        ref e => panic!("Expected Created event, got {:?}", e),
    }

    watcher.stop().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "filesystem event notification can be flaky"]
async fn test_watch_file_deletion() {
    let temp_dir = TempDir::new().unwrap();
    let skills_dir = temp_dir.path().join("skills");
    fs::create_dir_all(&skills_dir).unwrap();

    // Create a file to delete
    let skill_file = skills_dir.join("to_delete.md");
    fs::write(&skill_file, "test").unwrap();

    let (mut watcher, mut events) = SkillWatcher::new(&skills_dir).unwrap();
    watcher.start().unwrap();

    // Delete the file
    tokio::time::sleep(Duration::from_millis(200)).await;
    fs::remove_file(&skill_file).unwrap();

    // Wait for event
    let event = wait_for_event(&mut events, 5).await.unwrap();

    match event {
        WatchEvent::Removed(path) => {
            assert_eq!(path, skill_file);
        }
        ref e => panic!("Expected Removed event, got {:?}", e),
    }

    watcher.stop().unwrap();
}
