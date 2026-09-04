//! Skill definitions for the markdown-based skill system.

use crate::ids::SkillId;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    pub id: SkillId,
    pub metadata: SkillMetadata,
    pub instructions: String,
    pub examples: Vec<SkillExample>,
    pub constraints: Option<String>,
    pub content: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillMetadata {
    pub name: String,
    pub description: String,
    pub version: String,
    pub author: Option<String>,
    pub category: String,
    pub tags: Vec<String>,
    pub capabilities: Vec<String>,
    pub requirements: Vec<String>,
    pub triggers: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillExample {
    pub input: String,
    pub output: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillMatch {
    pub skill: Skill,
    pub score: f32,
    pub match_reasons: Vec<MatchReason>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MatchReason {
    TriggerMatch { trigger: String },
    TagMatch { tag: String },
    CapabilityMatch { capability: String },
    SemanticSimilarity { score: f32 },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_skill_serialization() {
        let skill = Skill {
            id: SkillId::new(),
            metadata: SkillMetadata {
                name: "rust-refactoring".to_string(),
                description: "Rust code refactoring".to_string(),
                version: "1.0.0".to_string(),
                author: None,
                category: "coding".to_string(),
                tags: vec!["rust".to_string()],
                capabilities: vec!["refactoring".to_string()],
                requirements: vec![],
                triggers: vec!["refactor rust".to_string()],
            },
            instructions: "You are a Rust expert.".to_string(),
            examples: vec![],
            constraints: None,
            content: "Full content".to_string(),
            path: PathBuf::from("/skills/rust.md"),
        };

        let json = serde_json::to_string(&skill).unwrap();
        let deserialized: Skill = serde_json::from_str(&json).unwrap();
        assert_eq!(skill.metadata.name, deserialized.metadata.name);
    }
}
