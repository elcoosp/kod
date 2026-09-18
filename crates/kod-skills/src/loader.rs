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
pub async fn load_from_dirs(dirs: &[std::path::PathBuf]) -> Result<Vec<kod_types::Skill>> {
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

#[cfg(test)]
mod coverage_multi_dir_loading {
    //! `load_from_dirs` is the entry point the CLI and TUI use. The
    //! shadowing contract — later directories override earlier ones
    //! by name — is what makes "project-local overrides global"
    //! work; a regression that flipped the order or appended rather
    //! than inserted would silently serve the wrong skill to a
    //! project that explicitly overrode it.
    use super::*;
    use tempfile::TempDir;

    fn write_skill(dir: &std::path::Path, name: &str, body: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join(format!("{name}.md")),
            format!(
                "---\nname: {name}\ndescription: desc for {name}\nversion: 1.0.0\ncategory: test\n---\n\n{body}\n",
            ),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn load_from_dirs_merges_every_directory() {
        let tmp = TempDir::new().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        write_skill(&a, "alpha", "A body");
        write_skill(&b, "beta", "B body");
        let skills = load_from_dirs(&[a, b]).await.unwrap();
        assert_eq!(skills.len(), 2);
        let names: Vec<&str> = skills.iter().map(|s| s.metadata.name.as_str()).collect();
        assert!(names.contains(&"alpha"));
        assert!(names.contains(&"beta"));
    }

    #[tokio::test]
    async fn later_directory_shadows_earlier_by_name() {
        let tmp = TempDir::new().unwrap();
        let global = tmp.path().join("global");
        let project = tmp.path().join("project");
        write_skill(&global, "same", "GLOBAL BODY");
        write_skill(&project, "same", "PROJECT BODY");
        let skills = load_from_dirs(&[global, project]).await.unwrap();
        assert_eq!(skills.len(), 1, "expected one surviving skill");
        assert!(
            skills[0].instructions.contains("PROJECT BODY"),
            "project did not shadow global: {}",
            skills[0].instructions,
        );
    }

    #[tokio::test]
    async fn nonexistent_directory_is_skipped_without_error() {
        let tmp = TempDir::new().unwrap();
        let real = tmp.path().join("real");
        let missing = tmp.path().join("does-not-exist");
        write_skill(&real, "alpha", "body");
        let skills = load_from_dirs(&[real, missing]).await.unwrap();
        assert_eq!(skills.len(), 1);
    }

    #[tokio::test]
    async fn empty_directory_list_returns_empty() {
        let skills = load_from_dirs(&[]).await.unwrap();
        assert!(skills.is_empty());
    }

    #[tokio::test]
    async fn malformed_file_in_one_dir_does_not_hide_a_good_file_elsewhere() {
        let tmp = TempDir::new().unwrap();
        let bad = tmp.path().join("bad");
        let good = tmp.path().join("good");
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(bad.join("broken.md"), "no front matter at all").unwrap();
        write_skill(&good, "good", "body");
        let skills = load_from_dirs(&[bad, good]).await.unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].metadata.name, "good");
    }
}

/// Coverage for the `SkillLoader` methods the existing tests do not
/// reach: `search`, `get_all_skills`, `remove_skill`, `reload_skill`,
/// `skills_dir`, `count` on empty, the error paths in `load_all`, and
/// `enable_hot_reload` idempotence.
#[cfg(test)]
mod coverage_loader_api {
    use super::*;
    use tempfile::TempDir;

    fn write_skill(dir: &std::path::Path, name: &str, description: &str) -> std::path::PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(format!("{name}.md"));
        std::fs::write(
            &path,
            format!(
                "---\nname: {name}\ndescription: {description}\n\
                 version: 1.0.0\ncategory: test\n\
                 tags:\n  - demo\n  - loader\n\
                 capabilities:\n  - search\n\
                 ---\n\n## Instructions\n\nbody for {name}\n",
            ),
        )
        .unwrap();
        path
    }

    // ---- skills_dir ---------------------------------------------------

    #[test]
    fn skills_dir_returns_the_construction_argument() {
        let loader = SkillLoader::new("/tmp/kod-loader-skills-dir");
        assert_eq!(
            loader.skills_dir(),
            std::path::Path::new("/tmp/kod-loader-skills-dir"),
        );
    }

    // ---- load_all error path -----------------------------------------

    #[tokio::test]
    async fn load_all_on_a_missing_directory_errors() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("does-not-exist");
        let mut loader = SkillLoader::new(&missing);
        let err = loader.load_all().await.unwrap_err();
        // The error must name the directory so the user can see
        // which config entry pointed at nothing.
        assert!(
            err.to_string().contains("does-not-exist")
                || err.to_string().contains("does not exist"),
            "error must name the missing directory: {err}",
        );
    }

    #[tokio::test]
    async fn load_all_on_an_empty_directory_returns_no_skills() {
        let tmp = TempDir::new().unwrap();
        let empty = tmp.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        let mut loader = SkillLoader::new(&empty);
        let skills = loader.load_all().await.unwrap();
        assert!(skills.is_empty());
        assert_eq!(loader.count().await, 0);
    }

    #[tokio::test]
    async fn load_all_clears_the_cache_before_reloading() {
        // The cache is cleared on every `load_all`, so a skill that
        // was deleted from disk between calls must not survive in
        // the cache.
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("skills");
        write_skill(&dir, "first", "first body");
        let mut loader = SkillLoader::new(&dir);
        loader.load_all().await.unwrap();
        assert_eq!(loader.count().await, 1);

        // Delete the file, reload, cache must be empty.
        std::fs::remove_file(dir.join("first.md")).unwrap();
        loader.load_all().await.unwrap();
        assert_eq!(
            loader.count().await,
            0,
            "load_all must clear the previous cache",
        );
    }

    #[tokio::test]
    async fn load_all_skips_malformed_files() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("skills");
        write_skill(&dir, "good", "good body");
        std::fs::write(dir.join("broken.md"), "no front matter").unwrap();
        let mut loader = SkillLoader::new(&dir);
        let skills = loader.load_all().await.unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].metadata.name, "good");
    }

    #[tokio::test]
    async fn load_all_ignores_non_md_files() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("skills");
        write_skill(&dir, "real", "body");
        std::fs::write(dir.join("notes.txt"), "some text").unwrap();
        std::fs::write(dir.join("data.json"), "{}").unwrap();
        let mut loader = SkillLoader::new(&dir);
        let skills = loader.load_all().await.unwrap();
        assert_eq!(skills.len(), 1);
    }

    // ---- get_skill / count --------------------------------------------

    #[tokio::test]
    async fn get_skill_returns_none_for_an_unknown_name() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("skills");
        write_skill(&dir, "present", "body");
        let mut loader = SkillLoader::new(&dir);
        loader.load_all().await.unwrap();
        assert!(loader.get_skill("absent").await.is_none());
        assert!(loader.get_skill("present").await.is_some());
    }

    // ---- get_all_skills -----------------------------------------------

    #[tokio::test]
    async fn get_all_skills_returns_every_cached_skill() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("skills");
        write_skill(&dir, "a", "a body");
        write_skill(&dir, "b", "b body");
        write_skill(&dir, "c", "c body");
        let mut loader = SkillLoader::new(&dir);
        loader.load_all().await.unwrap();
        let all = loader.get_all_skills().await;
        assert_eq!(all.len(), 3);
    }

    #[tokio::test]
    async fn get_all_skills_on_a_fresh_loader_is_empty() {
        let loader = SkillLoader::new("/tmp/unused");
        assert!(loader.get_all_skills().await.is_empty());
    }

    // ---- search -------------------------------------------------------

    #[tokio::test]
    async fn search_matches_on_name() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("skills");
        write_skill(&dir, "rust-helper", "some description");
        write_skill(&dir, "python-helper", "some description");
        let mut loader = SkillLoader::new(&dir);
        loader.load_all().await.unwrap();
        let hits = loader.search("rust").await;
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].metadata.name, "rust-helper");
    }

    #[tokio::test]
    async fn search_matches_on_description() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("skills");
        write_skill(&dir, "one", "handles CSV parsing");
        write_skill(&dir, "two", "handles JSON parsing");
        let mut loader = SkillLoader::new(&dir);
        loader.load_all().await.unwrap();
        let hits = loader.search("csv").await;
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].metadata.name, "one");
    }

    #[tokio::test]
    async fn search_matches_on_tags_and_capabilities() {
        // The helper writes `tags: [demo, loader]` and
        // `capabilities: [search]` for every skill. Both must be
        // searchable.
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("skills");
        write_skill(&dir, "anything", "body");
        let mut loader = SkillLoader::new(&dir);
        loader.load_all().await.unwrap();
        assert_eq!(loader.search("demo").await.len(), 1, "tag hit");
        assert_eq!(loader.search("loader").await.len(), 1, "tag hit");
        assert_eq!(loader.search("search").await.len(), 1, "capability hit");
    }

    #[tokio::test]
    async fn search_is_case_insensitive() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("skills");
        write_skill(&dir, "UpperName", "body");
        let mut loader = SkillLoader::new(&dir);
        loader.load_all().await.unwrap();
        assert_eq!(loader.search("uppername").await.len(), 1);
        assert_eq!(loader.search("UPPERNAME").await.len(), 1);
    }

    #[tokio::test]
    async fn search_with_no_matches_returns_empty() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("skills");
        write_skill(&dir, "present", "body");
        let mut loader = SkillLoader::new(&dir);
        loader.load_all().await.unwrap();
        assert!(loader.search("nothingmatchesthis").await.is_empty());
    }

    #[tokio::test]
    async fn search_with_empty_query_matches_every_skill() {
        // The empty string is contained in every string, so every
        // skill matches. A regression that special-cased the empty
        // query to return nothing would be defensible but is not
        // what the implementation does.
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("skills");
        write_skill(&dir, "a", "a body");
        write_skill(&dir, "b", "b body");
        let mut loader = SkillLoader::new(&dir);
        loader.load_all().await.unwrap();
        assert_eq!(loader.search("").await.len(), 2);
    }

    // ---- reload_skill -------------------------------------------------

    #[tokio::test]
    async fn reload_skill_updates_the_cache() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("skills");
        let path = write_skill(&dir, "target", "old description");
        let mut loader = SkillLoader::new(&dir);
        loader.load_all().await.unwrap();
        assert!(loader.get_skill("target").await.is_some());

        // Rewrite with a new description and reload just that file.
        std::fs::write(
            &path,
            "---\nname: target\ndescription: NEW DESCRIPTION\n\
             version: 1.0.0\ncategory: test\n---\n\nbody\n",
        )
        .unwrap();
        let reloaded = loader.reload_skill(&path).await.unwrap();
        assert!(reloaded.is_some());
        let cached = loader.get_skill("target").await.unwrap();
        assert_eq!(cached.metadata.description, "NEW DESCRIPTION");
    }

    #[tokio::test]
    async fn reload_skill_on_a_missing_file_errors() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("skills");
        std::fs::create_dir_all(&dir).unwrap();
        let loader = SkillLoader::new(&dir);
        let missing = dir.join("absent.md");
        assert!(loader.reload_skill(&missing).await.is_err());
    }

    // ---- remove_skill -------------------------------------------------

    #[tokio::test]
    async fn remove_skill_drops_it_from_the_cache_by_path() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("skills");
        let path = write_skill(&dir, "removable", "body");
        let mut loader = SkillLoader::new(&dir);
        loader.load_all().await.unwrap();
        assert_eq!(loader.count().await, 1);

        let removed = loader.remove_skill(&path).await;
        assert!(removed.is_some(), "remove_skill must return the removed skill");
        assert_eq!(removed.unwrap().metadata.name, "removable");
        assert_eq!(loader.count().await, 0);
    }

    #[tokio::test]
    async fn remove_skill_on_an_unknown_path_returns_none() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("skills");
        write_skill(&dir, "present", "body");
        let mut loader = SkillLoader::new(&dir);
        loader.load_all().await.unwrap();

        let unknown = tmp.path().join("nope.md");
        let result = loader.remove_skill(&unknown).await;
        assert!(result.is_none());
        // The cache is unchanged.
        assert_eq!(loader.count().await, 1);
    }

    // ---- enable_hot_reload --------------------------------------------

    #[tokio::test]
    async fn enable_hot_reload_is_idempotent() {
        // Calling twice must not error or spawn a second watcher.
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("skills");
        std::fs::create_dir_all(&dir).unwrap();
        let mut loader = SkillLoader::new(&dir);
        loader.enable_hot_reload().await.unwrap();
        loader.enable_hot_reload().await.unwrap();
    }

    #[tokio::test]
    async fn enable_hot_reload_on_a_missing_directory_errors() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("no-such-dir");
        let mut loader = SkillLoader::new(&missing);
        assert!(loader.enable_hot_reload().await.is_err());
    }
}
