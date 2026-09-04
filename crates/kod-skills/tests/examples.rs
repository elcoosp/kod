use kod_skills::{SkillLoader, SkillMatcher};

#[tokio::test]
async fn test_example_skills_load() {
    // Path relative to test execution
    let skills_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../skills/examples");

    let mut loader = SkillLoader::new(skills_dir);
    let skills = loader.load_all().await.unwrap();

    // Should have loaded 3 example skills
    assert_eq!(skills.len(), 3);

    let names: Vec<String> = skills
        .iter()
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

#[tokio::test]
async fn test_example_skills_match() {
    let skills_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../skills/examples");

    let mut loader = SkillLoader::new(skills_dir);
    let skills = loader.load_all().await.unwrap();

    let matcher = SkillMatcher::new();
    for skill in skills {
        matcher.add_skill(skill).await;
    }

    // Test trigger-based matching
    let matches = matcher.find_relevant_skills("refactor rust code").await;
    assert!(matches.iter().any(|m| m.skill.metadata.name == "rust-refactoring"));

    let matches = matcher.find_relevant_skills("write pytest tests").await;
    assert!(matches.iter().any(|m| m.skill.metadata.name == "python-testing"));
}
