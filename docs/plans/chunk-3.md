# Chunk 3: Skills System Implementation

## Task 12: Skill Parser (YAML Front Matter + Markdown)

**Files:**
- Modify: `crates/kod-skills/Cargo.toml`
- Create: `crates/kod-skills/src/lib.rs`
- Create: `crates/kod-skills/src/parser.rs`
- Test: `crates/kod-skills/tests/parser.rs`

- [ ] **Step 1: Update kod-skills Cargo.toml with dependencies**

```toml
[package]
name = "kod-skills"
version.workspace = true
edition.workspace = true
license.workspace = true
authors.workspace = true

[dependencies]
serde = { workspace = true }
serde_json = { workspace = true }
serde_yaml = "0.9"
walkdir = { workspace = true }
notify = { workspace = true }
tokio = { workspace = true }
tracing = { workspace = true }
kod-types = { path = "../kod-types" }
kod-error = { path = "../kod-error" }
kod-config = { path = "../kod-config" }

[dev-dependencies]
rstest = { workspace = true }
tempfile = "3.8"
```

- [ ] **Step 2: Write failing test for skill parsing**

Create `crates/kod-skills/tests/parser.rs`:

```rust
use kod_skills::parser::{parse_skill_content, SkillParser};
use kod_types::SkillId;

const VALID_SKILL = r#"---
name: rust-refactoring
description: Rust code refactoring with LSP integration
version: 1.2.0
author: kod-team
category: coding
tags:
  - rust
  - refactoring
  - lsp
capabilities:
  - code-refactoring
  - symbol-rename
requirements:
  - rust-analyzer
  - cargo
triggers:
  - "refactor rust"
  - "rename symbol"
---

# Rust Refactoring Skill

## Instructions

You are an expert Rust refactoring assistant. When activated:

1. **Analyze the code structure** using LSP tools
2. **Identify refactoring opportunities** based on Rust idioms
3. **Propose changes** with explanations
4. **Apply changes** using hash-anchored edits
5. **Verify changes** don't break references

## Examples

<example input="Refactor this function to use iterators">
Before:
```rust
fn sum_numbers(nums: &Vec<i32>) -> i32 {
    let mut sum = 0;
    for num in nums {
        sum += num;
    }
    sum
}
```

After:
```rust
fn sum_numbers(nums: &[i32]) -> i32 {
    nums.iter().sum()
}
```
</example>

## Constraints

- Never change public API without explicit request
- Preserve existing tests
"#;

#[test]
fn test_parse_valid_skill() {
    let parser = SkillParser::new();
    let skill = parser.parse_content(VALID_SKILL, "test.md").unwrap();
    
    assert_eq!(skill.metadata.name, "rust-refactoring");
    assert_eq!(skill.metadata.version, "1.2.0");
    assert_eq!(skill.metadata.author, Some("kod-team".to_string()));
    assert_eq!(skill.metadata.category, "coding");
    
    assert_eq!(skill.metadata.tags.len(), 3);
    assert!(skill.metadata.tags.contains(&"rust".to_string()));
    assert!(skill.metadata.tags.contains(&"refactoring".to_string()));
    
    assert_eq!(skill.metadata.capabilities.len(), 2);
    assert!(skill.metadata.capabilities.contains(&"code-refactoring".to_string()));
    
    assert_eq!(skill.metadata.triggers.len(), 2);
    assert!(skill.metadata.triggers.contains(&"refactor rust".to_string()));
}

#[test]
fn test_parse_instructions() {
    let parser = SkillParser::new();
    let skill = parser.parse_content(VALID_SKILL, "test.md").unwrap();
    
    assert!(skill.instructions.contains("expert Rust refactoring assistant"));
    assert!(skill.instructions.contains("LSP tools"));
    assert!(!skill.instructions.contains("Examples"));
}

#[test]
fn test_parse_constraints() {
    let parser = SkillParser::new();
    let skill = parser.parse_content(VALID_SKILL, "test.md").unwrap();
    
    let constraints = skill.constraints.expect("Should have constraints");
    assert!(constraints.contains("Never change public API"));
    assert!(constraints.contains("Preserve existing tests"));
}

#[test]
fn test_skill_id_generated() {
    let parser = SkillParser::new();
    let skill = parser.parse_content(VALID_SKILL, "test.md").unwrap();
    
    assert!(!skill.id.as_uuid().is_nil());
}

#[test]
fn test_parse_invalid_yaml() {
    let invalid = "---\ninvalid: yaml: [unclosed\n---\n# Body";
    let parser = SkillParser::new();
    let result = parser.parse_content(invalid, "test.md");
    
    assert!(result.is_err());
}

#[test]
fn test_parse_missing_front_matter() {
    let no_front_matter = "# Just markdown without front matter";
    let parser = SkillParser::new();
    let result = parser.parse_content(no_front_matter, "test.md");
    
    assert!(result.is_err());
}
```

- [ ] **Step 3: Run tests to verify they fail**

```bash
cargo test -p kod-skills --test parser
```

Expected: FAIL - parser module not implemented

- [ ] **Step 4: Implement skill parser**

Create `crates/kod-skills/src/lib.rs`:

```rust
//! Skills system for markdown-based skill management.
//!
//! This crate handles loading, parsing, matching, and hot-reloading
//! of skill files from ~/.kod/skills/

pub mod parser;
pub mod loader;
pub mod matcher;
pub mod watcher;

pub use parser::{SkillParser, ParsedSkill};
pub use loader::SkillLoader;
pub use matcher::SkillMatcher;
```

Create `crates/kod-skills/src/parser.rs`:

```rust
//! Skill file parsing - extracts metadata from YAML front matter
//! and structured sections from markdown body.

use kod_error::{KodError, Result};
use kod_types::{Skill, SkillExample, SkillId, SkillMetadata};
use std::path::{Path, PathBuf};

/// Parser for skill markdown files
#[derive(Debug, Default)]
pub struct SkillParser {
    /// Additional validation rules
    strict_mode: bool,
}

impl SkillParser {
    pub fn new() -> Self {
        Self { strict_mode: false }
    }

    pub fn strict(mut self) -> Self {
        self.strict_mode = true;
        self
    }

    /// Parse a skill from a file
    pub fn parse_file(&self, path: &Path) -> Result<Skill> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| KodError::SkillParseError {
                path: path.display().to_string(),
                reason: format!("Failed to read file: {}", e),
            })?;
        
        self.parse_content(&content, path.display().to_string())
            .map(|mut skill| {
                skill.path = path.to_path_buf();
                skill
            })
    }

    /// Parse skill content from a string
    pub fn parse_content(&self, content: &str, source_name: &str) -> Result<Skill> {
        let (metadata, body) = self.split_front_matter(content, source_name)?;
        
        self.validate_metadata(&metadata)?;
        
        let instructions = self.extract_section(&body, "## Instructions")
            .ok_or_else(|| KodError::SkillParseError {
                path: source_name.to_string(),
                reason: "Missing '## Instructions' section".to_string(),
            })?;
        
        let examples = self.extract_examples(&body);
        let constraints = self.extract_section(&body, "## Constraints");
        
        Ok(Skill {
            id: SkillId::new(),
            metadata,
            instructions,
            examples,
            constraints,
            content: body.to_string(),
            path: PathBuf::from(source_name),
        })
    }

    /// Split content into front matter and body
    fn split_front_matter(&self, content: &str, source: &str) -> Result<(SkillMetadata, String)> {
        let content = content.trim();
        
        if !content.starts_with("---") {
            return Err(KodError::SkillParseError {
                path: source.to_string(),
                reason: "Missing YAML front matter (must start with ---)".to_string(),
            });
        }
        
        // Find the closing --- marker
        let rest = &content[3..];
        let end_pos = rest.find("\n---")
            .ok_or_else(|| KodError::SkillParseError {
                path: source.to_string(),
                reason: "Missing closing --- marker for front matter".to_string(),
            })?;
        
        let yaml_str = &rest[..end_pos];
        let body = rest[end_pos + 4..].trim().to_string();
        
        // Parse YAML
        let metadata: SkillMetadata = serde_yaml::from_str(yaml_str.trim())
            .map_err(|e| KodError::SkillParseError {
                path: source.to_string(),
                reason: format!("Invalid YAML: {}", e),
            })?;
        
        Ok((metadata, body))
    }

    /// Validate metadata has required fields
    fn validate_metadata(&self, metadata: &SkillMetadata) -> Result<()> {
        if metadata.name.is_empty() {
            return Err(KodError::SkillValidationFailed {
                reason: "Skill name is required".to_string(),
            });
        }
        
        if metadata.description.is_empty() {
            return Err(KodError::SkillValidationFailed {
                reason: "Skill description is required".to_string(),
            });
        }
        
        if metadata.version.is_empty() {
            if self.strict_mode {
                return Err(KodError::SkillValidationFailed {
                    reason: "Skill version is required in strict mode".to_string(),
                });
            }
            // Default version is handled by Default impl
        }
        
        Ok(())
    }

    /// Extract a section from markdown body
    fn extract_section(&self, body: &str, section_header: &str) -> Option<String> {
        let start = body.find(section_header)?;
        let content_after = &body[start + section_header.len()..];
        
        // Find next section or end
        let end = content_after.find("\n## ")
            .map(|pos| &content_after[..pos])
            .unwrap_or(content_after);
        
        Some(end.trim().to_string())
    }

    /// Extract examples from <example> tags
    fn extract_examples(&self, body: &str) -> Vec<SkillExample> {
        let mut examples = Vec::new();
        
        // Find all <example>...</example> blocks
        let mut search_pos = 0;
        
        while let Some(start_rel) = body[search_pos..].find("<example") {
            let start = search_pos + start_rel;
            
            // Extract attributes from the opening tag
            let tag_end = body[start..].find('>')
                .map(|pos| start + pos + 1)?;
            
            let opening_tag = &body[start..tag_end];
            let input_attr = self.extract_attribute(opening_tag, "input");
            
            // Find closing tag
            let close_rel = body[tag_end..].find("</example>")
                .map(|pos| tag_end + pos)?;
            
            let content = &body[tag_end..close_rel];
            
            examples.push(SkillExample {
                input: input_attr.unwrap_or_default(),
                output: content.trim().to_string(),
            });
            
            search_pos = close_rel + "</example>".len();
        }
        
        examples
    }

    /// Extract attribute value from an XML-like tag
    fn extract_attribute(&self, tag: &str, attr: &str) -> Option<String> {
        let pattern = format!("{}=\"", attr);
        let start = tag.find(&pattern)?;
        let content_start = start + pattern.len();
        let end = tag[content_start..].find('"')? + content_start;
        
        Some(tag[content_start..end].to_string())
    }
}

/// Intermediate parsing result (used internally)
#[derive(Debug)]
pub struct ParsedSkill {
    pub metadata: SkillMetadata,
    pub body: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_SKILL = r#"---
name: test-skill
description: A test skill
version: 1.0.0
category: testing
tags:
  - test
capabilities:
  - testing
triggers:
  - "test trigger"
---

# Test Skill

## Instructions

Test instructions here.

## Constraints

Test constraints.
"#;

    #[test]
    fn test_full_parse() {
        let parser = SkillParser::new();
        let skill = parser.parse_content(TEST_SKILL, "test.md").unwrap();
        
        assert_eq!(skill.metadata.name, "test-skill");
        assert!(skill.instructions.contains("Test instructions"));
        assert!(skill.constraints.unwrap().contains("Test constraints"));
    }

    #[test]
    fn test_no_constraints() {
        let content = r#"---
name: test
description: Test
version: 1.0.0
category: test
---

## Instructions

Just instructions.
"#;
        
        let parser = SkillParser::new();
        let skill = parser.parse_content(content, "test.md").unwrap();
        assert!(skill.constraints.is_none());
    }
}
```

- [ ] **Step 5: Run tests to verify they pass**

```bash
cargo test -p kod-skills --test parser
cargo test -p kod-skills --lib parser
```

Expected: All tests pass

- [ ] **Step 6: Commit**

```bash
git add crates/kod-skills/
git commit -m "feat(skills): add skill parser with YAML front matter and markdown sections"
```

---

## Task 13: Skill Loader with Directory Scanning

**Files:**
- Create: `crates/kod-skills/src/loader.rs`
- Test: `crates/kod-skills/tests/loader.rs`

- [ ] **Step 1: Write failing test for skill loader**

Create `crates/kod-skills/tests/loader.rs`:

```rust
use kod_skills::loader::SkillLoader;
use std::fs;
use tempfile::TempDir;

fn create_test_skills_dir() -> TempDir {
    let temp_dir = TempDir::new().unwrap();
    let skills_dir = temp_dir.path().join("skills");
    fs::create_dir_all(&skills_dir).unwrap();
    
    // Create test skill files
    let rust_skill = r#"---
name: rust-refactoring
description: Rust refactoring skill
version: 1.0.0
category: coding
tags:
  - rust
capabilities:
  - refactoring
triggers:
  - "refactor rust"
---

## Instructions

Rust refactoring instructions.
"#;
    
    let python_skill = r#"---
name: python-testing
description: Python testing skill
version: 1.0.0
category: coding
tags:
  - python
  - testing
capabilities:
  - testing
triggers:
  - "test python"
---

## Instructions

Python testing instructions.
"#;
    
    // Create subdirectories for organization
    let coding_dir = skills_dir.join("coding");
    fs::create_dir_all(&coding_dir).unwrap();
    
    fs::write(coding_dir.join("rust.md"), rust_skill).unwrap();
    fs::write(coding_dir.join("python.md"), python_skill).unwrap();
    
    // Create an invalid file (should be skipped)
    fs::write(skills_dir.join("invalid.md"), "not a valid skill").unwrap();
    
    // Create a non-markdown file (should be ignored)
    fs::write(skills_dir.join("notes.txt"), "some notes").unwrap();
    
    temp_dir
}

#[tokio::test]
async fn test_load_skills_from_directory() {
    let temp_dir = create_test_skills_dir();
    let skills_dir = temp_dir.path().join("skills");
    
    let mut loader = SkillLoader::new(skills_dir);
    let skills = loader.load_all().await.unwrap();
    
    // Should load 2 valid skills, skip invalid and non-md files
    assert_eq!(skills.len(), 2);
    
    let names: Vec<String> = skills.iter()
        .map(|s| s.metadata.name.clone())
        .collect();
    
    assert!(names.contains(&"rust-refactoring".to_string()));
    assert!(names.contains(&"python-testing".to_string()));
}

#[tokio::test]
async fn test_get_skill_by_name() {
    let temp_dir = create_test_skills_dir();
    let skills_dir = temp_dir.path().join("skills");
    
    let mut loader = SkillLoader::new(skills_dir);
    loader.load_all().await.unwrap();
    
    let skill = loader.get_skill("rust-refactoring").await;
    assert!(skill.is_some());
    
    let skill = skill.unwrap();
    assert_eq!(skill.metadata.category, "coding");
}

#[tokio::test]
async fn test_get_skill_not_found() {
    let temp_dir = create_test_skills_dir();
    let skills_dir = temp_dir.path().join("skills");
    
    let mut loader = SkillLoader::new(skills_dir);
    loader.load_all().await.unwrap();
    
    let skill = loader.get_skill("nonexistent").await;
    assert!(skill.is_none());
}

#[tokio::test]
async fn test_search_skills() {
    let temp_dir = create_test_skills_dir();
    let skills_dir = temp_dir.path().join("skills");
    
    let mut loader = SkillLoader::new(skills_dir);
    loader.load_all().await.unwrap();
    
    // Search by tag
    let results = loader.search("rust").await;
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].metadata.name, "rust-refactoring");
    
    // Search by description
    let results = loader.search("testing").await;
    assert_eq!(results.len(), 1);
    
    // Search by capability
    let results = loader.search("refactoring").await;
    assert_eq!(results.len(), 1);
}

#[tokio::test]
async fn test_empty_directory() {
    let temp_dir = TempDir::new().unwrap();
    let skills_dir = temp_dir.path().join("skills");
    fs::create_dir_all(&skills_dir).unwrap();
    
    let mut loader = SkillLoader::new(skills_dir);
    let skills = loader.load_all().await.unwrap();
    
    assert_eq!(skills.len(), 0);
}

#[tokio::test]
async fn test_nonexistent_directory() {
    let temp_dir = TempDir::new().unwrap();
    let skills_dir = temp_dir.path().join("nonexistent");
    
    let mut loader = SkillLoader::new(skills_dir);
    let result = loader.load_all().await;
    
    assert!(result.is_err());
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test -p kod-skills --test loader
```

Expected: FAIL - loader module not implemented

- [ ] **Step 3: Implement skill loader**

Create `crates/kod-skills/src/loader.rs`:

```rust
//! Skill loader - scans directories and loads skill files into memory.

use crate::parser::SkillParser;
use kod_error::{KodError, Result};
use kod_types::Skill;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio::sync::RwLock;
use walkdir::WalkDir;

/// Loader that manages skills in memory
pub struct SkillLoader {
    skills_dir: PathBuf,
    cache: RwLock<HashMap<String, Skill>>,
    parser: SkillParser,
}

impl SkillLoader {
    /// Create a new loader for the given directory
    pub fn new(skills_dir: impl Into<PathBuf>) -> Self {
        Self {
            skills_dir: skills_dir.into(),
            cache: RwLock::new(HashMap::new()),
            parser: SkillParser::new(),
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
        
        cache.values()
            .filter(|skill| {
                skill.metadata.name.to_lowercase().contains(&query_lower)
                    || skill.metadata.description.to_lowercase().contains(&query_lower)
                    || skill.metadata.tags.iter().any(|t| t.to_lowercase().contains(&query_lower))
                    || skill.metadata.capabilities.iter().any(|c| c.to_lowercase().contains(&query_lower))
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
        drop(cache);
        
        if let Some(skill) = skill {
            let mut cache = self.cache.write().await;
            cache.remove(&skill.metadata.name);
        }
        
        skill
    }

    /// Get skills directory path
    pub fn skills_dir(&self) -> &Path {
        &self.skills_dir
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
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
cargo test -p kod-skills --test loader
cargo test -p kod-skills --lib loader
```

Expected: All tests pass

- [ ] **Step 5: Commit**

```bash
git add crates/kod-skills/
git commit -m "feat(skills): add skill loader with directory scanning and caching"
```

---

## Task 14: Hot Reloading with File Watcher

**Files:**
- Create: `crates/kod-skills/src/watcher.rs`
- Modify: `crates/kod-skills/src/loader.rs`
- Test: `crates/kod-skills/tests/watcher.rs`

- [ ] **Step 1: Write failing test for hot reloading**

Create `crates/kod-skills/tests/watcher.rs`:

```rust
use kod_skills::loader::SkillLoader;
use kod_skills::watcher::SkillWatcher;
use std::fs;
use std::time::Duration;
use tempfile::TempDir;

#[tokio::test]
async fn test_hot_reload_on_file_change() {
    let temp_dir = TempDir::new().unwrap();
    let skills_dir = temp_dir.path().join("skills");
    fs::create_dir_all(&skills_dir).unwrap();
    
    // Create initial skill
    let skill_file = skills_dir.join("test.md");
    fs::write(&skill_file, r#"---
name: test
description: Original description
version: 1.0.0
category: test
---

## Instructions

Original.
"#).unwrap();
    
    // Set up loader and watcher
    let mut loader = SkillLoader::new(&skills_dir);
    loader.load_all().await.unwrap();
    
    let (watcher, mut events) = SkillWatcher::new(&skills_dir).unwrap();
    watcher.start().unwrap();
    
    // Modify the file
    tokio::time::sleep(Duration::from_millis(100)).await;
    fs::write(&skill_file, r#"---
name: test
description: Updated description
version: 1.0.1
category: test
---

## Instructions

Updated.
"#).unwrap();
    
    // Wait for watcher to detect change
    let timeout = Duration::from_secs(5);
    let event = tokio::time::timeout(timeout, events.recv()).await;
    
    assert!(event.is_ok(), "Should receive file change event");
    
    let event = event.unwrap().unwrap();
    match event {
        kod_skills::watcher::WatchEvent::Modified(path) => {
            assert_eq!(path, skill_file);
        }
        _ => panic!("Expected Modified event"),
    }
    
    // Cleanup
    watcher.stop().unwrap();
}

#[tokio::test]
async fn test_watch_new_file() {
    let temp_dir = TempDir::new().unwrap();
    let skills_dir = temp_dir.path().join("skills");
    fs::create_dir_all(&skills_dir).unwrap();
    
    let (watcher, mut events) = SkillWatcher::new(&skills_dir).unwrap();
    watcher.start().unwrap();
    
    // Create a new skill file
    tokio::time::sleep(Duration::from_millis(100)).await;
    let new_skill = skills_dir.join("new.md");
    fs::write(&new_skill, r#"---
name: new
description: New skill
version: 1.0.0
category: test
---

## Instructions

New.
"#).unwrap();
    
    // Wait for event
    let timeout = Duration::from_secs(5);
    let event = tokio::time::timeout(timeout, events.recv()).await;
    
    assert!(event.is_ok());
    let event = event.unwrap().unwrap();
    match event {
        kod_skills::watcher::WatchEvent::Created(path) => {
            assert_eq!(path, new_skill);
        }
        _ => panic!("Expected Created event"),
    }
    
    watcher.stop().unwrap();
}

#[tokio::test]
async fn test_watch_file_deletion() {
    let temp_dir = TempDir::new().unwrap();
    let skills_dir = temp_dir.path().join("skills");
    fs::create_dir_all(&skills_dir).unwrap();
    
    // Create a file to delete
    let skill_file = skills_dir.join("to_delete.md");
    fs::write(&skill_file, "test").unwrap();
    
    let (watcher, mut events) = SkillWatcher::new(&skills_dir).unwrap();
    watcher.start().unwrap();
    
    // Delete the file
    tokio::time::sleep(Duration::from_millis(100)).await;
    fs::remove_file(&skill_file).unwrap();
    
    // Wait for event
    let timeout = Duration::from_secs(5);
    let event = tokio::time::timeout(timeout, events.recv()).await;
    
    assert!(event.is_ok());
    let event = event.unwrap().unwrap();
    match event {
        kod_skills::watcher::WatchEvent::Removed(path) => {
            assert_eq!(path, skill_file);
        }
        _ => panic!("Expected Removed event"),
    }
    
    watcher.stop().unwrap();
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test -p kod-skills --test watcher
```

Expected: FAIL - watcher module not implemented

- [ ] **Step 3: Implement file watcher**

Create `crates/kod-skills/src/watcher.rs`:

```rust
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
        
        watcher.watch(watch_dir, RecursiveMode::Recursive)
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
        self.is_running.store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
    
    /// Stop the watcher
    pub fn stop(&self) -> Result<()> {
        self.is_running.store(false, std::sync::atomic::Ordering::SeqCst);
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
    
    #[test]
    fn test_watcher_creation() {
        let temp_dir = TempDir::new().unwrap();
        let skills_dir = temp_dir.path().join("skills");
        std::fs::create_dir_all(&skills_dir).unwrap();
        
        let (watcher, _rx) = SkillWatcher::new(&skills_dir).unwrap();
        assert_eq!(watcher.watch_dir(), skills_dir);
    }
}
```

- [ ] **Step 4: Add hot reload integration to loader**

Update `crates/kod-skills/src/loader.rs` to add hot reload support:

```rust
//! Skill loader - scans directories and loads skill files into memory.

use crate::parser::SkillParser;
use crate::watcher::{SkillWatcher, WatchEvent};
use kod_error::{KodError, Result};
use kod_types::Skill;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio::sync::RwLock;
use walkdir::WalkDir;

/// Loader that manages skills in memory with hot reload support
pub struct SkillLoader {
    skills_dir: PathBuf,
    cache: RwLock<HashMap<String, Skill>>,
    parser: SkillParser,
    watcher: Option<SkillWatcher>,
}

impl SkillLoader {
    /// Create a new loader for the given directory
    pub fn new(skills_dir: impl Into<PathBuf>) -> Self {
        Self {
            skills_dir: skills_dir.into(),
            cache: RwLock::new(HashMap::new()),
            parser: SkillParser::new(),
            watcher: None,
        }
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
        let cache = self.cache.clone();
        let parser = self.parser.clone();
        
        tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                handle_watch_event(event, &cache, &parser).await;
            }
        });
        
        Ok(())
    }
    
    // ... rest of the implementation remains the same
}

/// Handle a watch event (reload or remove skill)
async fn handle_watch_event(
    event: WatchEvent,
    cache: &RwLock<HashMap<String, Skill>>,
    parser: &SkillParser,
) {
    match event {
        WatchEvent::Created(path) | WatchEvent::Modified(path) => {
            match parser.parse_file(&path) {
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
            }
        }
        WatchEvent::Removed(path) => {
            let mut cache_guard = cache.write().await;
            // Find and remove skill by path
            let skill_to_remove = cache_guard.values()
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
```

- [ ] **Step 5: Run tests**

```bash
cargo test -p kod-skills --test watcher
cargo test -p kod-skills --lib watcher
```

Expected: All tests pass (watcher tests may be flaky on some systems)

- [ ] **Step 6: Commit**

```bash
git add crates/kod-skills/
git commit -m "feat(skills): add hot reloading with file system watcher"
```

---

## Task 15: Skill Matcher (Pattern-Based)

**Files:**
- Create: `crates/kod-skills/src/matcher.rs`
- Test: `crates/kod-skills/tests/matcher.rs`

- [ ] **Step 1: Write failing test for skill matcher**

Create `crates/kod-skills/tests/matcher.rs`:

```rust
use kod_skills::matcher::SkillMatcher;
use kod_types::{Skill, SkillId, SkillMetadata};
use std::path::PathBuf;

fn create_test_skill(name: &str, tags: Vec<&str>, capabilities: Vec<&str>, triggers: Vec<&str>) -> Skill {
    Skill {
        id: SkillId::new(),
        metadata: SkillMetadata {
            name: name.to_string(),
            description: format!("Skill for {}", name),
            version: "1.0.0".to_string(),
            author: None,
            category: "test".to_string(),
            tags: tags.into_iter().map(String::from).collect(),
            capabilities: capabilities.into_iter().map(String::from).collect(),
            requirements: Vec::new(),
            triggers: triggers.into_iter().map(String::from).collect(),
        },
        instructions: "Test instructions".to_string(),
        examples: Vec::new(),
        constraints: None,
        content: "Test content".to_string(),
        path: PathBuf::from("/test/skill.md"),
    }
}

#[tokio::test]
async fn test_match_by_trigger() {
    let mut matcher = SkillMatcher::new();
    
    let skill = create_test_skill(
        "rust-skill",
        vec!["rust"],
        vec!["refactoring"],
        vec!["refactor rust code", "rust refactoring"]
    );
    
    matcher.add_skill(skill);
    
    // Query that matches a trigger
    let matches = matcher.find_relevant_skills("please refactor rust code for me").await;
    
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].skill.metadata.name, "rust-skill");
    assert!(matches[0].score > 0.5);
    assert!(!matches[0].match_reasons.is_empty());
}

#[tokio::test]
async fn test_match_by_tags() {
    let mut matcher = SkillMatcher::new();
    
    let skill = create_test_skill(
        "python-skill",
        vec!["python", "testing"],
        vec!["testing"],
        vec!["test python"]
    );
    
    matcher.add_skill(skill);
    
    // Query that mentions a tag
    let matches = matcher.find_relevant_skills("I need help with python testing").await;
    
    assert_eq!(matches.len(), 1);
    assert!(matches[0].score > 0.3);
}

#[tokio::test]
async fn test_match_by_capabilities() {
    let mut matcher = SkillMatcher::new();
    
    let skill = create_test_skill(
        "api-skill",
        vec!["api", "rest"],
        vec!["api-design", "documentation"],
        vec!["design api"]
    );
    
    matcher.add_skill(skill);
    
    // Query that mentions a capability
    let matches = matcher.find_relevant_skills("help me with api-design patterns").await;
    
    assert_eq!(matches.len(), 1);
}

#[tokio::test]
async fn test_no_match() {
    let mut matcher = SkillMatcher::new();
    
    let skill = create_test_skill(
        "rust-skill",
        vec!["rust"],
        vec!["refactoring"],
        vec!["refactor rust"]
    );
    
    matcher.add_skill(skill);
    
    // Query that doesn't match
    let matches = matcher.find_relevant_skills("make me a sandwich").await;
    
    assert_eq!(matches.len(), 0);
}

#[tokio::test]
async fn test_multiple_matches_ranked() {
    let mut matcher = SkillMatcher::new();
    
    // Add two skills with different relevance
    let high_relevance = create_test_skill(
        "rust-expert",
        vec!["rust"],
        vec!["refactoring"],
        vec!["refactor rust code"]
    );
    
    let low_relevance = create_test_skill(
        "general-coding",
        vec!["coding", "general"],
        vec!["general"],
        vec!["help with code"]
    );
    
    matcher.add_skill(high_relevance);
    matcher.add_skill(low_relevance);
    
    // Query that strongly matches the first skill
    let matches = matcher.find_relevant_skills("refactor rust code").await;
    
    assert_eq!(matches.len(), 2);
    
    // Higher relevance should be first
    assert_eq!(matches[0].skill.metadata.name, "rust-expert");
    assert!(matches[0].score > matches[1].score);
}

#[tokio::test]
async fn test_limit_results() {
    let mut matcher = SkillMatcher::new();
    matcher.set_max_results(2);
    
    // Add multiple skills
    for i in 0..5 {
        let skill = create_test_skill(
            &format!("skill-{}", i),
            vec!["test"],
            vec!["testing"],
            vec!["test"]
        );
        matcher.add_skill(skill);
    }
    
    let matches = matcher.find_relevant_skills("test").await;
    
    // Should limit to 2 results
    assert_eq!(matches.len(), 2);
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test -p kod-skills --test matcher
```

Expected: FAIL - matcher module not implemented

- [ ] **Step 3: Implement skill matcher**

Create `crates/kod-skills/src/matcher.rs`:

```rust
//! Skill matcher - finds relevant skills based on user input.
//!
//! Uses pattern matching (triggers, tags, capabilities) to score
//! and rank skills by relevance.

use kod_types::{MatchReason, Skill, SkillMatch};
use std::collections::HashMap;
use tokio::sync::RwLock;

/// Matches user queries to relevant skills
pub struct SkillMatcher {
    skills: RwLock<HashMap<String, Skill>>,
    max_results: usize,
    min_score: f32,
}

impl SkillMatcher {
    pub fn new() -> Self {
        Self {
            skills: RwLock::new(HashMap::new()),
            max_results: 3,
            min_score: 0.3,
        }
    }

    /// Set maximum number of results to return
    pub fn set_max_results(&mut self, max: usize) {
        self.max_results = max;
    }

    /// Set minimum score threshold
    pub fn set_min_score(&mut self, min: f32) {
        self.min_score = min;
    }

    /// Add a skill to the matcher
    pub async fn add_skill(&self, skill: Skill) {
        self.skills.write().await.insert(skill.metadata.name.clone(), skill);
    }

    /// Remove a skill from the matcher
    pub async fn remove_skill(&self, name: &str) {
        self.skills.write().await.remove(name);
    }

    /// Find skills relevant to the given query
    pub async fn find_relevant_skills(&self, query: &str) -> Vec<SkillMatch> {
        let query_lower = query.to_lowercase();
        let skills = self.skills.read().await;
        
        let mut matches: Vec<SkillMatch> = Vec::new();
        
        for skill in skills.values() {
            let (score, reasons) = self.score_skill(skill, &query_lower);
            
            if score >= self.min_score {
                matches.push(SkillMatch {
                    skill: skill.clone(),
                    score,
                    match_reasons: reasons,
                });
            }
        }
        
        // Sort by score (descending)
        matches.sort_by(|a, b| {
            b.score.partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        
        // Limit results
        matches.into_iter().take(self.max_results).collect()
    }

    /// Score a single skill against a query
    fn score_skill(&self, skill: &Skill, query: &str) -> (f32, Vec<MatchReason>) {
        let mut score = 0.0;
        let mut reasons = Vec::new();
        
        // 1. Trigger matching (highest weight: 0.8)
        for trigger in &skill.metadata.triggers {
            let trigger_lower = trigger.to_lowercase();
            if query.contains(&trigger_lower) {
                score += 0.8;
                reasons.push(MatchReason::TriggerMatch {
                    trigger: trigger.clone(),
                });
            }
        }
        
        // 2. Tag matching (medium weight: 0.4)
        for tag in &skill.metadata.tags {
            let tag_lower = tag.to_lowercase();
            if query.contains(&tag_lower) || tag_lower.contains(query) {
                score += 0.4;
                reasons.push(MatchReason::TagMatch {
                    tag: tag.clone(),
                });
            }
        }
        
        // 3. Capability matching (lower weight: 0.3)
        for capability in &skill.metadata.capabilities {
            let cap_lower = capability.to_lowercase();
            if query.contains(&cap_lower) {
                score += 0.3;
                reasons.push(MatchReason::CapabilityMatch {
                    capability: capability.clone(),
                });
            }
        }
        
        // 4. Name matching (if query is similar to skill name)
        let name_lower = skill.metadata.name.to_lowercase();
        if query.contains(&name_lower) || name_lower.contains(query) {
            score += 0.5;
        }
        
        // Normalize score to 0.0 - 1.0
        score = score.min(1.0);
        
        (score, reasons)
    }

    /// Get all skills in the matcher
    pub async fn get_all_skills(&self) -> Vec<Skill> {
        self.skills.read().await.values().cloned().collect()
    }

    /// Get number of skills
    pub async fn count(&self) -> usize {
        self.skills.read().await.len()
    }
}

impl Default for SkillMatcher {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kod_types::{SkillId, SkillMetadata};
    use std::path::PathBuf;

    fn create_skill(name: &str, triggers: Vec<&str>) -> Skill {
        Skill {
            id: SkillId::new(),
            metadata: SkillMetadata {
                name: name.to_string(),
                description: "Test".to_string(),
                version: "1.0.0".to_string(),
                author: None,
                category: "test".to_string(),
                tags: Vec::new(),
                capabilities: Vec::new(),
                requirements: Vec::new(),
                triggers: triggers.into_iter().map(String::from).collect(),
            },
            instructions: "Test".to_string(),
            examples: Vec::new(),
            constraints: None,
            content: String::new(),
            path: PathBuf::new(),
        }
    }

    #[tokio::test]
    async fn test_basic_matching() {
        let matcher = SkillMatcher::new();
        matcher.add_skill(create_skill("test", vec!["test trigger"])).await;
        
        let results = matcher.find_relevant_skills("test trigger").await;
        assert_eq!(results.len(), 1);
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
cargo test -p kod-skills --test matcher
cargo test -p kod-skills --lib matcher
```

Expected: All tests pass

- [ ] **Step 5: Commit**

```bash
git add crates/kod-skills/
git commit -m "feat(skills): add pattern-based skill matcher with scoring and ranking"
```

---

## Task 16: Integration - Skills with Loader and Matcher

**Files:**
- Create: `crates/kod-skills/src/lib.rs` (updated)
- Test: `crates/kod-skills/tests/integration.rs`

- [ ] **Step 1: Write integration test for skills system**

Create `crates/kod-skills/tests/integration.rs`:

```rust
use kod_skills::{loader::SkillLoader, matcher::SkillMatcher};
use std::fs;
use tempfile::TempDir;

const SKILL_CONTENT = r#"---
name: rust-refactoring
description: Rust code refactoring
version: 1.0.0
category: coding
tags:
  - rust
  - refactoring
capabilities:
  - code-refactoring
triggers:
  - "refactor rust"
  - "rust code"
---

## Instructions

Rust refactoring instructions here.
"#;

#[tokio::test]
async fn test_full_skills_pipeline() {
    // 1. Set up test directory
    let temp_dir = TempDir::new().unwrap();
    let skills_dir = temp_dir.path().join("skills");
    fs::create_dir_all(&skills_dir).unwrap();
    
    // Create skill file
    fs::write(skills_dir.join("rust.md"), SKILL_CONTENT).unwrap();
    
    // 2. Load skills
    let mut loader = SkillLoader::new(&skills_dir);
    let skills = loader.load_all().await.unwrap();
    
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].metadata.name, "rust-refactoring");
    
    // 3. Add skills to matcher
    let matcher = SkillMatcher::new();
    for skill in skills {
        matcher.add_skill(skill).await;
    }
    
    // 4. Find matching skill
    let matches = matcher.find_relevant_skills("please refactor rust code").await;
    
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].skill.metadata.name, "rust-refactoring");
    assert!(matches[0].score > 0.5);
    
    // 5. Test no match
    let no_matches = matcher.find_relevant_skills("make coffee").await;
    assert_eq!(no_matches.len(), 0);
}

#[tokio::test]
async fn test_skill_retrieval_by_name() {
    let temp_dir = TempDir::new().unwrap();
    let skills_dir = temp_dir.path().join("skills");
    fs::create_dir_all(&skills_dir).unwrap();
    
    fs::write(skills_dir.join("test.md"), SKILL_CONTENT).unwrap();
    
    let mut loader = SkillLoader::new(&skills_dir);
    loader.load_all().await.unwrap();
    
    // Get by exact name
    let skill = loader.get_skill("rust-refactoring").await;
    assert!(skill.is_some());
    
    let skill = skill.unwrap();
    assert_eq!(skill.metadata.category, "coding");
    assert!(skill.instructions.contains("Rust refactoring"));
    
    // Get by non-existent name
    let not_found = loader.get_skill("nonexistent").await;
    assert!(not_found.is_none());
}
```

- [ ] **Step 2: Update lib.rs to ensure proper module exports**

Update `crates/kod-skills/src/lib.rs`:

```rust
//! Skills system for markdown-based skill management.
//!
//! This crate handles loading, parsing, matching, and hot-reloading
//! of skill files from ~/.kod/skills/
//!
//! # Example
//!
//! ```rust
//! use kod_skills::{SkillLoader, SkillMatcher};
//!
//! # async fn example() {
//! let mut loader = SkillLoader::new("~/.kod/skills");
//! let skills = loader.load_all().await.unwrap();
//!
//! let matcher = SkillMatcher::new();
//! for skill in skills {
//!     matcher.add_skill(skill).await;
//! }
//!
//! let matches = matcher.find_relevant_skills("refactor rust code").await;
//! # }
//! ```

pub mod parser;
pub mod loader;
pub mod matcher;
pub mod watcher;

pub use parser::SkillParser;
pub use loader::SkillLoader;
pub use matcher::SkillMatcher;
pub use watcher::{SkillWatcher, WatchEvent};
```

- [ ] **Step 3: Run all kod-skills tests**

```bash
cargo test -p kod-skills
```

Expected: All tests pass

- [ ] **Step 4: Verify workspace still builds**

```bash
cargo build --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

Expected: Build succeeds with no warnings

- [ ] **Step 5: Commit integration**

```bash
git add crates/kod-skills/
git commit -m "feat(skills): integrate loader, parser, matcher, and watcher"
```

---

## Task 17: Create Example Skills for Testing

**Files:**
- Create: `skills/examples/rust-refactoring.md`
- Create: `skills/examples/python-testing.md`
- Create: `skills/examples/code-review.md`

- [ ] **Step 1: Create example skill directory structure**

```bash
mkdir -p skills/examples
```

- [ ] **Step 2: Create rust-refactoring skill**

Create `skills/examples/rust-refactoring.md`:

```markdown
---
name: rust-refactoring
description: Rust code refactoring with idiomatic patterns and best practices
version: 1.0.0
author: kod-team
category: coding
tags:
  - rust
  - refactoring
  - idioms
capabilities:
  - code-refactoring
  - pattern-matching
  - ownership-analysis
requirements:
  - rust-analyzer
triggers:
  - "refactor rust"
  - "rust refactoring"
  - "improve rust code"
  - "make more idiomatic"
---

# Rust Refactoring Skill

## Instructions

You are an expert Rust refactoring assistant. When activated:

1. **Analyze the code structure** for anti-patterns
2. **Identify opportunities** for more idiomatic Rust
3. **Consider ownership and borrowing** implications
4. **Propose changes** with clear explanations
5. **Verify changes** maintain API compatibility

### Common Refactoring Patterns

1. **Iterator chains** over explicit loops
2. **`Option`/`Result` combinators** over match statements
3. **`impl Trait`** over generic parameters when appropriate
4. **`Cow<str>`** for string flexibility
5. **Builder pattern** for complex construction

## Examples

<example input="Refactor this loop to use iterators">
Before:
```rust
fn sum_squares(nums: &Vec<i32>) -> i32 {
    let mut sum = 0;
    for num in nums {
        sum += num * num;
    }
    sum
}
```

After:
```rust
fn sum_squares(nums: &[i32]) -> i32 {
    nums.iter().map(|n| n * n).sum()
}
```
</example>

<example input="Replace match with map_and_then">
Before:
```rust
fn process_value(value: Option<i32>) -> Option<String> {
    match value {
        Some(v) => {
            if v > 0 {
                Some(v.to_string())
            } else {
                None
            }
        }
        None => None,
    }
}
```

After:
```rust
fn process_value(value: Option<i32>) -> Option<String> {
    value.filter(|v| *v > 0).map(|v| v.to_string())
}
```
</example>

## Constraints

- Never change public API without explicit request
- Preserve existing tests and their behavior
- Maintain error handling semantics
- Consider performance implications
- Warn about breaking changes
```

- [ ] **Step 3: Create python-testing skill**

Create `skills/examples/python-testing.md`:

```markdown
---
name: python-testing
description: Python testing best practices with pytest
version: 1.0.0
author: kod-team
category: testing
tags:
  - python
  - testing
  - pytest
capabilities:
  - test-generation
  - test-refactoring
  - mock-setup
requirements:
  - pytest
triggers:
  - "write tests"
  - "python testing"
  - "pytest"
  - "unit tests"
---

# Python Testing Skill

## Instructions

You are an expert Python testing assistant. When activated:

1. **Analyze the code** to understand testable components
2. **Identify edge cases** and boundary conditions
3. **Write comprehensive tests** using pytest
4. **Use fixtures** for common setup
5. **Apply mocking** where appropriate

### Testing Guidelines

- Use descriptive test names that explain behavior
- Test one concept per test
- Use `pytest.mark.parametrize` for multiple inputs
- Mock external dependencies
- Test both success and failure paths

## Examples

<example input="Write tests for a function that divides numbers">
```python
import pytest
from calculator import divide

class TestDivide:
    def test_divide_positive_numbers(self):
        assert divide(10, 2) == 5.0
    
    def test_divide_negative_numbers(self):
        assert divide(-10, 2) == -5.0
    
    def test_divide_by_zero_raises(self):
        with pytest.raises(ZeroDivisionError):
            divide(10, 0)
    
    @pytest.mark.parametrize("a,b,expected", [
        (1, 1, 1.0),
        (100, 10, 10.0),
        (0, 5, 0.0),
    ])
    def test_divide_parametrized(self, a, b, expected):
        assert divide(a, b) == expected
```
</example>

## Constraints

- Tests must be independent and isolated
- Use fixtures over setup/teardown methods
- Mock external I/O and network calls
- Maintain test coverage above 80%
- Follow existing project conventions
```

- [ ] **Step 4: Create code-review skill**

Create `skills/examples/code-review.md`:

```markdown
---
name: code-review
description: Comprehensive code review with security and performance analysis
version: 1.0.0
author: kod-team
category: review
tags:
  - review
  - security
  - performance
  - quality
capabilities:
  - code-analysis
  - security-review
  - performance-review
requirements: []
triggers:
  - "review code"
  - "code review"
  - "check code quality"
  - "security review"
---

# Code Review Skill

## Instructions

You are an expert code reviewer. When activated:

1. **Analyze code quality** using established metrics
2. **Check for security vulnerabilities**
3. **Evaluate performance implications**
4. **Assess maintainability and readability**
5. **Provide actionable feedback**

### Review Categories

1. **Correctness** - Does the code work as intended?
2. **Security** - Are there vulnerabilities?
3. **Performance** - Are there inefficiencies?
4. **Readability** - Is the code clear and well-documented?
5. **Maintainability** - Will it be easy to modify?

## Examples

<example input="Review this function for issues">
```rust
fn process_input(input: &str) -> Result<String, Box<dyn std::error::Error>> {
    let data = input.parse::<i32>()?;
    let result = 100 / data;
    Ok(result.to_string())
}
```

Issues found:
1. **Division by zero** - No check for `data == 0`
2. **Error handling** - Using `Box<dyn Error>` instead of specific error type
3. **Performance** - String allocation for simple conversion

Suggested fix:
```rust
fn process_input(input: &str) -> Result<String, ProcessingError> {
    let data: i32 = input.parse()?;
    let result = 100.checked_div(data)
        .ok_or(ProcessingError::DivisionByZero)?;
    Ok(result.to_string())
}
```
</example>

## Constraints

- Always provide constructive feedback
- Prioritize security issues over style
- Include code examples for fixes
- Consider the project's context
- Be specific, not generic
```

- [ ] **Step 5: Test example skills load correctly**

Create `crates/kod-skills/tests/examples.rs`:

```rust
use kod_skills::{SkillLoader, SkillMatcher};

#[tokio::test]
async fn test_example_skills_load() {
    // Path relative to test execution
    let skills_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../skills/examples");
    
    let mut loader = SkillLoader::new(skills_dir);
    let skills = loader.load_all().await.unwrap();
    
    // Should have loaded 3 example skills
    assert_eq!(skills.len(), 3);
    
    let names: Vec<String> = skills.iter()
        .map(|s| s.metadata.name.clone())
        .collect();
    
    assert!(names.contains(&"rust-refactoring".to_string()));
    assert!(names.contains(&"python-testing".to_string()));
    assert!(names.contains(&"code-review".to_string()));
}

#[tokio::test]
async fn test_example_skills_have_instructions() {
    let skills_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../skills/examples");
    
    let mut loader = SkillLoader::new(skills_dir);
    let skills = loader.load_all().await.unwrap();
    
    for skill in skills {
        assert!(
            !skill.instructions.is_empty(),
            "Skill '{}' should have instructions",
            skill.metadata.name
        );
    }
}
```

- [ ] **Step 6: Run example tests**

```bash
cargo test -p kod-skills --test examples
```

Expected: All tests pass

- [ ] **Step 7: Commit example skills**

```bash
git add skills/ crates/kod-skills/tests/examples.rs
git commit -m "feat(skills): add example skills for testing and documentation"
```

---

## Chunk 3 Review Checklist

- [ ] Skill parser correctly handles YAML front matter
- [ ] Skill parser extracts Instructions, Examples, and Constraints sections
- [ ] Skill loader scans directories recursively for .md files
- [ ] Skill loader handles invalid files gracefully (skips them)
- [ ] Hot reloading works with file system watcher
- [ ] Skill matcher scores and ranks skills by relevance
- [ ] Integration between loader, parser, and matcher works
- [ ] Example skills demonstrate proper format
- [ ] All tests pass
- [ ] Clippy passes with no warnings

**Verification commands:**

```bash
cargo test -p kod-skills
cargo clippy -p kod-skills -- -D warnings
cargo build --workspace
```

---

## Chunk 3 Summary

**Implemented:**
1. **Skill Parser** (`parser.rs`)
   - YAML front matter extraction
   - Markdown section parsing (Instructions, Constraints)
   - Example extraction from `<example>` tags
   - Metadata validation

2. **Skill Loader** (`loader.rs`)
   - Recursive directory scanning
   - In-memory caching with RwLock
   - Search by name, description, tags, capabilities
   - Hot reload integration

3. **File Watcher** (`watcher.rs`)
   - notify-based file system watching
   - Event handling (Create, Modify, Remove)
   - Integration with loader for hot reload

4. **Skill Matcher** (`matcher.rs`)
   - Pattern-based matching (triggers, tags, capabilities)
   - Scoring and ranking system
   - Configurable thresholds and result limits

5. **Example Skills**
   - rust-refactoring skill with full examples
   - python-testing skill with pytest patterns
   - code-review skill with review guidelines

**Next Chunk Preview:**

Chunk 4 will cover the **Memory System** implementation:
- Short-term memory (in-memory with capacity limits)
- Long-term memory (redb-backed persistent storage)
- Episodic memory (vector-based for semantic search)
- Memory manager that coordinates all types
- Context builder that assembles memory context

Would you like me to continue with **Chunk 4: Memory System**?
