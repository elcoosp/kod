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

    let mut loader = SkillLoader::new(&skills_dir);
    let skills = loader.load_all().await.unwrap();

    // Should load 2 valid skills, skip invalid and non-md files
    assert_eq!(skills.len(), 2);

    let names: Vec<String> = skills.iter().map(|s| s.metadata.name.clone()).collect();

    assert!(names.contains(&"rust-refactoring".to_string()));
    assert!(names.contains(&"python-testing".to_string()));
}

#[tokio::test]
async fn test_get_skill_by_name() {
    let temp_dir = create_test_skills_dir();
    let skills_dir = temp_dir.path().join("skills");

    let mut loader = SkillLoader::new(&skills_dir);
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

    let mut loader = SkillLoader::new(&skills_dir);
    loader.load_all().await.unwrap();

    let skill = loader.get_skill("nonexistent").await;
    assert!(skill.is_none());
}

#[tokio::test]
async fn test_search_skills() {
    let temp_dir = create_test_skills_dir();
    let skills_dir = temp_dir.path().join("skills");

    let mut loader = SkillLoader::new(&skills_dir);
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

    let mut loader = SkillLoader::new(&skills_dir);
    let skills = loader.load_all().await.unwrap();

    assert_eq!(skills.len(), 0);
}

#[tokio::test]
async fn test_nonexistent_directory() {
    let temp_dir = TempDir::new().unwrap();
    let skills_dir = temp_dir.path().join("nonexistent");

    let mut loader = SkillLoader::new(&skills_dir);
    let result = loader.load_all().await;

    assert!(result.is_err());
}
