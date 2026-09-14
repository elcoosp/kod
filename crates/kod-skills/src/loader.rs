//! Skill loader - scans directories and loads skill files into memory.

use crate::parser::SkillParser;
use crate::watcher::{SkillWatcher, WatchEvent};
use kod_error::{KodError, Result};
use kod_types::Skill;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::RwLock;
use walkdir::WalkDir;

/// Loader that manages skills in memory with hot reload support
pub struct SkillLoader {
    skills_dir: PathBuf,
    cache: Arc<RwLock<HashMap<String, Skill>>>,
    parser: SkillParser,
    watcher: Option<SkillWatcher>,
}

impl SkillLoader {
    /// Create a new loader for the given directory
    pub fn new(skills_dir: impl Into<PathBuf>) -> Self {
        Self {
            skills_dir: skills_dir.into(),
            cache: Arc::new(RwLock::new(HashMap::new())),
            parser: SkillParser::new(),
            watcher: None,
        }
    }

    /// Load all skills from the directory
    pub async fn load_all(&mut self) -> Result<Vec<Skill>> {
        let skills = self.scan_directory().await?;

        // Update cache
        let mut cache = self.cache.write().await;
        cache.clear();

        for skill in &skills {
            cache.insert(skill.metadata.name.clone(), skill.clone());
        }

        Ok(skills)
    }

    /// Scan directory recursively for .md files
    async fn scan_directory(&self) -> Result<Vec<Skill>> {
        if !self.skills_dir.exists() {
            return Err(KodError::SkillParseError {
                path: self.skills_dir.display().to_string(),
                reason: "Skills directory does not exist".to_string(),
            });
        }

        let mut skills = Vec::new();

        for entry in WalkDir::new(&self.skills_dir)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
        {
            let path = entry.path();

            // Only process .md files
            if path.extension().and_then(|s| s.to_str()) != Some("md") {
                continue;
            }

            match self.parser.parse_file(path) {
                Ok(skill) => {
                    tracing::debug!(
                        path = %path.display(),
                        name = %skill.metadata.name,
                        "Loaded skill"
                    );
                    skills.push(skill);
                }
                Err(e) => {
                    // Log error but continue loading other skills
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "Failed to parse skill file"
                    );
                }
            }
        }

        Ok(skills)
    }

    /// Get a skill by name
    pub async fn get_skill(&self, name: &str) -> Option<Skill> {
        self.cache.read().await.get(name).cloned()
    }

    /// Search skills by query (matches name, description, tags, capabilities)
    pub async fn search(&self, query: &str) -> Vec<Skill> {
        let query_lower = query.to_lowercase();
        let cache = self.cache.read().await;

        cache
            .values()
            .filter(|skill| {
                skill.metadata.name.to_lowercase().contains(&query_lower)
                    || skill
                        .metadata
                        .description
                        .to_lowercase()
                        .contains(&query_lower)
                    || skill
                        .metadata
                        .tags
                        .iter()
                        .any(|t| t.to_lowercase().contains(&query_lower))
                    || skill
                        .metadata
                        .capabilities
                        .iter()
                        .any(|c| c.to_lowercase().contains(&query_lower))
            })
            .cloned()
            .collect()
    }

    /// Get all loaded skills
    pub async fn get_all_skills(&self) -> Vec<Skill> {
        self.cache.read().await.values().cloned().collect()
    }

    /// Get number of loaded skills
    pub async fn count(&self) -> usize {
        self.cache.read().await.len()
    }

    /// Reload a specific skill file
    pub async fn reload_skill(&self, path: &Path) -> Result<Option<Skill>> {
        let skill = self.parser.parse_file(path)?;

        let mut cache = self.cache.write().await;
        cache.insert(skill.metadata.name.clone(), skill.clone());

        Ok(Some(skill))
    }

    /// Remove a skill from cache (when file is deleted)
    pub async fn remove_skill(&self, path: &Path) -> Option<Skill> {
        // Find skill by path
        let cache = self.cache.read().await;
        let skill = cache.values().find(|s| s.path == path).cloned();
        let name = skill.as_ref().map(|s| s.metadata.name.clone());
        drop(cache);

        if let Some(name) = name {
            let mut cache = self.cache.write().await;
            cache.remove(&name);
        }

        skill
    }

    /// Get skills directory path
    pub fn skills_dir(&self) -> &Path {
        &self.skills_dir
    }

    /// Enable hot reloading
    pub async fn enable_hot_reload(&mut self) -> Result<()> {
        if self.watcher.is_some() {
            return Ok(()); // Already enabled
        }

        let (watcher, mut event_rx) = SkillWatcher::new(&self.skills_dir)?;
        watcher.start()?;

        self.watcher = Some(watcher);

        // Spawn task to handle hot reload events
        let cache = Arc::clone(&self.cache);
        let parser = self.parser.clone();

        tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                handle_watch_event(event, &cache, &parser).await;
            }
        });

        Ok(())
    }
}

/// Load all skills from multiple directories, deduplicating by name.
///
/// Later directories shadow earlier ones for skills that share a name,
/// so project-local skills override global ones. Directories that do not
/// exist are skipped with a warning — an empty home skills dir must not
/// block a session.
pub async fn load_from_dirs(
    dirs: &[std::path::PathBuf],
) -> Result<Vec<kod_types::Skill>> {
    use std::collections::HashMap;
    let mut by_name: HashMap<String, kod_types::Skill> = HashMap::new();
    for dir in dirs {
        if !dir.is_dir() {
            continue;
        }
        let mut loader = SkillLoader::new(dir);
        match loader.load_all().await {
            Ok(skills) => {
                for skill in skills {
                    by_name.insert(skill.metadata.name.clone(), skill);
                }
            }
            Err(e) => {
                tracing::warn!(
                    dir = %dir.display(),
                    error = %e,
                    "Could not load skills from directory"
                );
            }
        }
    }
    Ok(by_name.into_values().collect())
}

/// Handle a watch event (reload or remove skill)
async fn handle_watch_event(
    event: WatchEvent,
    cache: &RwLock<HashMap<String, Skill>>,
    parser: &SkillParser,
) {
    match event {
        WatchEvent::Created(path) | WatchEvent::Modified(path) => match parser.parse_file(&path) {
            Ok(skill) => {
                let mut cache_guard = cache.write().await;
                cache_guard.insert(skill.metadata.name.clone(), skill);
                tracing::info!(
                    path = %path.display(),
                    "Skill reloaded"
                );
            }
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "Failed to reload skill"
                );
            }
        },
        WatchEvent::Removed(path) => {
            let mut cache_guard = cache.write().await;
            // Find and remove skill by path
            let skill_to_remove = cache_guard
                .values()
                .find(|s| s.path == path)
                .map(|s| s.metadata.name.clone());

            if let Some(name) = skill_to_remove {
                cache_guard.remove(&name);
                tracing::info!(
                    path = %path.display(),
                    name = %name,
                    "Skill removed"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_loader_lifecycle() {
        let temp_dir = TempDir::new().unwrap();
        let skills_dir = temp_dir.path().join("skills");
        std::fs::create_dir_all(&skills_dir).unwrap();

        let skill_content = r#"---
name: test
description: Test skill
version: 1.0.0
category: test
---

## Instructions

Test.
"#;

        std::fs::write(skills_dir.join("test.md"), skill_content).unwrap();

        let mut loader = SkillLoader::new(&skills_dir);
        let skills = loader.load_all().await.unwrap();
        assert_eq!(skills.len(), 1);

        let skill = loader.get_skill("test").await;
        assert!(skill.is_some());

        let count = loader.count().await;
        assert_eq!(count, 1);
    }
}
