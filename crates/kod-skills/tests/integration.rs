use kod_skills::{SkillLoader, SkillMatcher};
use std::fs;
use tempfile::TempDir;

const SKILL_CONTENT: &str = r#"---
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
    let matches = matcher
        .find_relevant_skills("please refactor rust code")
        .await;

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
