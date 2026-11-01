//! Skill definitions for the markdown-based skill system.

use crate::ids::SkillId;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// A loaded skill.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    /// The skill's unique id.
    pub id: SkillId,
    /// Parsed frontmatter metadata.
    pub metadata: SkillMetadata,
    /// The skill's instruction body.
    pub instructions: String,
    /// Example input/output pairs.
    pub examples: Vec<SkillExample>,
    /// Optional constraints the model must respect.
    pub constraints: Option<String>,
    /// The full markdown body (frontmatter stripped).
    pub content: String,
    /// The file the skill was loaded from.
    pub path: PathBuf,
}

/// The frontmatter block at the top of a skill file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillMetadata {
    /// The skill's name.
    pub name: String,
    /// A one-line description.
    pub description: String,
    /// Semantic version string.
    #[serde(default)]
    pub version: String,
    /// Optional author.
    pub author: Option<String>,
    /// The skill's category.
    #[serde(default)]
    pub category: String,
    /// Free-form tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Capabilities the skill provides.
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Requirements the environment must satisfy.
    #[serde(default)]
    pub requirements: Vec<String>,
    /// Phrases that activate the skill.
    #[serde(default)]
    pub triggers: Vec<String>,
}

/// One example input/output pair for a skill.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillExample {
    /// The example input.
    pub input: String,
    /// The expected output.
    pub output: String,
}

/// A skill matched against the current turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillMatch {
    /// The matched skill.
    pub skill: Skill,
    /// The overall relevance score.
    pub score: f32,
    /// The individual reasons the skill matched.
    pub match_reasons: Vec<MatchReason>,
}

/// Why a skill matched the current turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MatchReason {
    /// A trigger phrase was present.
    TriggerMatch {
        /// The trigger phrase that matched.
        trigger: String,
    },
    /// A tag matched the turn.
    TagMatch {
        /// The tag that matched.
        tag: String,
    },
    /// A capability match was found.
    CapabilityMatch {
        /// The capability that matched.
        capability: String,
    },
    /// The skill name appeared in the turn.
    NameMatch {
        /// The name that matched.
        name: String,
    },
    /// Semantic similarity exceeded the threshold.
    SemanticSimilarity {
        /// The similarity score.
        score: f32,
    },
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

#[cfg(test)]
mod coverage_skill_metadata {
    //! `MatchReason` is what a `kod skills search` prints and what a
    //! future `/skills` diagnostic reads. Its serialized shape is
    //! the contract a downstream consumer depends on. The metadata
    //! defaults are the shape a hand-written skill file relies on:
    //! only `name` and `description` are required, everything else
    //! defaults.
    use super::*;

    #[test]
    fn match_reason_round_trips_every_variant() {
        let cases = [
            MatchReason::TriggerMatch {
                trigger: "refactor rust".into(),
            },
            MatchReason::TagMatch { tag: "rust".into() },
            MatchReason::CapabilityMatch {
                capability: "refactoring".into(),
            },
            MatchReason::NameMatch {
                name: "rust-refactoring".into(),
            },
            MatchReason::SemanticSimilarity { score: 0.42 },
        ];
        for r in &cases {
            let json = serde_json::to_string(r).unwrap();
            let parsed: MatchReason = serde_json::from_str(&json).unwrap();
            let re = serde_json::to_string(&parsed).unwrap();
            assert_eq!(json, re, "roundtrip mismatch: {json}");
        }
    }

    #[test]
    fn match_reason_uses_externally_tagged_shape() {
        // The default derive is externally tagged: the variant name
        // is the JSON key. A caller that inspects the JSON (a
        // script, a log viewer) relies on the exact spelling.
        let r = MatchReason::TriggerMatch {
            trigger: "x".into(),
        };
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.starts_with("{\"TriggerMatch\""), "got: {json}");
    }

    #[test]
    fn metadata_only_name_and_description_are_required() {
        // A hand-written skill file sets `name` and `description`;
        // every other field defaults. Removing `#[serde(default)]`
        // from any of them would make every minimal skill file fail
        // to parse.
        let json = r#"{"name":"a","description":"d"}"#;
        let m: SkillMetadata = serde_json::from_str(json).unwrap();
        assert_eq!(m.name, "a");
        assert_eq!(m.description, "d");
        assert!(m.version.is_empty());
        assert!(m.category.is_empty());
        assert!(m.tags.is_empty());
        assert!(m.capabilities.is_empty());
        assert!(m.requirements.is_empty());
        assert!(m.triggers.is_empty());
        assert!(m.author.is_none());
    }

    #[test]
    fn metadata_round_trips_every_field() {
        let m = SkillMetadata {
            name: "rust-refactoring".into(),
            description: "desc".into(),
            version: "1.2.3".into(),
            author: Some("alice".into()),
            category: "coding".into(),
            tags: vec!["a".into(), "b".into()],
            capabilities: vec!["x".into()],
            requirements: vec!["rust-analyzer".into()],
            triggers: vec!["refactor rust".into()],
        };
        let json = serde_json::to_string(&m).unwrap();
        let parsed: SkillMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(m.name, parsed.name);
        assert_eq!(m.version, parsed.version);
        assert_eq!(m.author, parsed.author);
        assert_eq!(m.tags, parsed.tags);
        assert_eq!(m.capabilities, parsed.capabilities);
        assert_eq!(m.requirements, parsed.requirements);
        assert_eq!(m.triggers, parsed.triggers);
    }

    #[test]
    fn skill_example_round_trips() {
        let e = SkillExample {
            input: "before".into(),
            output: "after".into(),
        };
        let json = serde_json::to_string(&e).unwrap();
        let parsed: SkillExample = serde_json::from_str(&json).unwrap();
        assert_eq!(e.input, parsed.input);
        assert_eq!(e.output, parsed.output);
    }

    #[test]
    fn skill_match_carries_reasons_and_score() {
        let s = Skill {
            id: SkillId::new(),
            metadata: SkillMetadata {
                name: "x".into(),
                description: "d".into(),
                version: String::new(),
                author: None,
                category: String::new(),
                tags: vec![],
                capabilities: vec![],
                requirements: vec![],
                triggers: vec![],
            },
            instructions: String::new(),
            examples: vec![],
            constraints: None,
            content: String::new(),
            path: std::path::PathBuf::new(),
        };
        let m = SkillMatch {
            skill: s.clone(),
            score: 0.75,
            match_reasons: vec![MatchReason::NameMatch { name: "x".into() }],
        };
        let json = serde_json::to_string(&m).unwrap();
        let parsed: SkillMatch = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.score, 0.75);
        assert_eq!(parsed.match_reasons.len(), 1);
        assert_eq!(parsed.skill.metadata.name, "x");
    }
}
