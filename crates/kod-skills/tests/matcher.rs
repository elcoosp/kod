use kod_skills::matcher::SkillMatcher;
use kod_types::{Skill, SkillId, SkillMetadata};
use std::path::PathBuf;

fn create_test_skill(
    name: &str,
    tags: Vec<&str>,
    capabilities: Vec<&str>,
    triggers: Vec<&str>,
) -> Skill {
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
    let matcher = SkillMatcher::new();

    let skill = create_test_skill(
        "rust-skill",
        vec!["rust"],
        vec!["refactoring"],
        vec!["refactor rust code", "rust refactoring"],
    );

    matcher.add_skill(skill).await;

    // Query that matches a trigger
    let matches = matcher.find_relevant_skills("please refactor rust code for me").await;

    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].skill.metadata.name, "rust-skill");
    assert!(matches[0].score > 0.5);
    assert!(!matches[0].match_reasons.is_empty());
}

#[tokio::test]
async fn test_match_by_tags() {
    let matcher = SkillMatcher::new();

    let skill = create_test_skill(
        "python-skill",
        vec!["python", "testing"],
        vec!["testing"],
        vec!["test python"],
    );

    matcher.add_skill(skill).await;

    // Query that mentions a tag
    let matches = matcher.find_relevant_skills("I need help with python testing").await;

    assert_eq!(matches.len(), 1);
    assert!(matches[0].score > 0.3);
}

#[tokio::test]
async fn test_match_by_capabilities() {
    let matcher = SkillMatcher::new();

    let skill = create_test_skill(
        "api-skill",
        vec!["api", "rest"],
        vec!["api-design", "documentation"],
        vec!["design api"],
    );

    matcher.add_skill(skill).await;

    // Query that mentions a capability
    let matches = matcher.find_relevant_skills("help me with api-design patterns").await;

    assert_eq!(matches.len(), 1);
}

#[tokio::test]
async fn test_no_match() {
    let matcher = SkillMatcher::new();

    let skill = create_test_skill(
        "rust-skill",
        vec!["rust"],
        vec!["refactoring"],
        vec!["refactor rust"],
    );

    matcher.add_skill(skill).await;

    // Query that doesn't match
    let matches = matcher.find_relevant_skills("make me a sandwich").await;

    assert_eq!(matches.len(), 0);
}

#[tokio::test]
async fn test_multiple_matches_ranked() {
    let matcher = SkillMatcher::new();

    // Add two skills with different relevance
    let high_relevance = create_test_skill(
        "rust-expert",
        vec!["rust"],
        vec!["refactoring"],
        vec!["refactor rust code"],
    );

    let low_relevance = create_test_skill(
        "general-coding",
        vec!["coding", "general"],
        vec!["general"],
        vec!["help with code"],
    );

    matcher.add_skill(high_relevance).await;
    matcher.add_skill(low_relevance).await;

    // Query that matches both skills
    let matches = matcher.find_relevant_skills("refactor rust code coding").await;

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
            vec!["test"],
        );
        matcher.add_skill(skill).await;
    }

    let matches = matcher.find_relevant_skills("test").await;

    // Should limit to 2 results
    assert_eq!(matches.len(), 2);
}
