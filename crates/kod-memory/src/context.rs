//! Context builder - assembles context from various sources for LLM prompts.
//!
//! Combines user input, memory, and skills into a structured context
//! that can be serialized into a prompt.

use kod_types::{MemoryContext, Skill};
use serde::{Deserialize, Serialize};

/// Built context ready for LLM processing
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Context {
    pub user_input: String,
    pub memory_context: MemoryContext,
    pub skills: Vec<Skill>,
    pub system_prompt: Option<String>,
}

impl Context {
    /// Estimate token count (rough approximation)
    pub fn estimated_tokens(&self) -> usize {
        let total_chars = self.user_input.len()
            + self
                .memory_context
                .working_memory
                .iter()
                .map(|m| m.content.len())
                .sum::<usize>()
            + self
                .memory_context
                .long_term
                .iter()
                .map(|m| m.content.len())
                .sum::<usize>()
            + self
                .skills
                .iter()
                .map(|s| s.instructions.len())
                .sum::<usize>();

        // Rough: 1 token ≈ 4 characters
        total_chars / 4
    }

    /// Convert to a prompt string for the LLM
    pub fn to_prompt(&self) -> String {
        let mut prompt = String::new();

        // Add system context from memory
        if !self.memory_context.working_memory.is_empty() {
            prompt.push_str("## Current Context\n\n");
            for entry in &self.memory_context.working_memory {
                prompt.push_str(&format!("- {}\n", entry.content));
            }
            prompt.push('\n');
        }

        if !self.memory_context.long_term.is_empty() {
            prompt.push_str("## User Preferences & Knowledge\n\n");
            for entry in &self.memory_context.long_term {
                prompt.push_str(&format!("- {}\n", entry.content));
            }
            prompt.push('\n');
        }

        // Add skill instructions
        if !self.skills.is_empty() {
            prompt.push_str("## Relevant Skills\n\n");
            for skill in &self.skills {
                prompt.push_str(&format!(
                    "### {}\n\n{}\n\n",
                    skill.metadata.name, skill.instructions
                ));
            }
        }

        // Add system prompt if provided
        if let Some(system) = &self.system_prompt {
            prompt.push_str(&format!("## System\n\n{}\n\n", system));
        }

        // Add user input
        prompt.push_str(&format!("## User Request\n\n{}", self.user_input));

        prompt
    }
}

/// Builder for Context
#[derive(Debug, Default)]
pub struct ContextBuilder {
    user_input: String,
    memory_context: MemoryContext,
    skills: Vec<Skill>,
    system_prompt: Option<String>,
}

impl ContextBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the user input
    pub fn with_user_input(mut self, input: impl Into<String>) -> Self {
        self.user_input = input.into();
        self
    }

    /// Set the memory context
    pub fn with_memory(mut self, memory: MemoryContext) -> Self {
        self.memory_context = memory;
        self
    }

    /// Add skills to the context
    pub fn with_skills(mut self, skills: Vec<Skill>) -> Self {
        self.skills = skills;
        self
    }

    /// Add a system prompt
    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = Some(prompt.into());
        self
    }

    /// Build the final context
    pub fn build(self) -> Context {
        Context {
            user_input: self.user_input,
            memory_context: self.memory_context,
            skills: self.skills,
            system_prompt: self.system_prompt,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_context_builder() {
        let context = ContextBuilder::new().with_user_input("Test input").build();

        assert_eq!(context.user_input, "Test input");
    }

    #[test]
    fn test_prompt_generation() {
        let context = ContextBuilder::new().with_user_input("Help me").build();

        let prompt = context.to_prompt();
        assert!(prompt.contains("Help me"));
        assert!(prompt.contains("## User Request"));
    }
}

#[cfg(test)]
mod coverage_memory_context {
    //! `Context` is a sibling of `kod-core::context::EngineContext`:
    //! the memory crate's own prompt shape. The two share names but
    //! not code; a caller that imported the wrong one gets a
    //! prompt with the wrong section order. The tests pin this
    //! crate's own section headings and ordering so the two shapes
    //! cannot drift into accidental agreement.
    use super::*;
    use kod_types::{MemoryEntry, MemoryId, MemoryType, Skill, SkillId, SkillMetadata};
    use std::path::PathBuf;
    use time::OffsetDateTime;

    fn entry(content: &str) -> MemoryEntry {
        MemoryEntry {
            id: MemoryId::new(),
            memory_type: MemoryType::ShortTerm,
            content: content.to_string(),
            timestamp: OffsetDateTime::now_utc(),
            relevance: 1.0,
            metadata: Default::default(),
        }
    }

    fn skill(name: &str, body: &str) -> Skill {
        Skill {
            id: SkillId::new(),
            metadata: SkillMetadata {
                name: name.to_string(),
                description: "d".to_string(),
                version: "1.0.0".to_string(),
                author: None,
                category: "test".to_string(),
                tags: vec![],
                capabilities: vec![],
                requirements: vec![],
                triggers: vec![],
            },
            instructions: body.to_string(),
            examples: vec![],
            constraints: None,
            content: String::new(),
            path: PathBuf::new(),
        }
    }

    #[test]
    fn empty_context_renders_only_the_user_request() {
        // No memory, no skills, no system prompt. The `to_prompt`
        // output is exactly the user-request section — a
        // regression that emitted empty headers would waste
        // tokens on every call.
        let c = Context::default();
        let p = c.to_prompt();
        assert!(p.contains("## User Request"));
        assert!(!p.contains("## Current Context"));
        assert!(!p.contains("## User Preferences & Knowledge"));
        assert!(!p.contains("## Relevant Skills"));
        assert!(!p.contains("## System"));
    }

    #[test]
    fn working_memory_renders_under_current_context() {
        let mut c = Context::default();
        c.memory_context.working_memory = vec![entry("first"), entry("second")];
        let p = c.to_prompt();
        assert!(p.contains("## Current Context"));
        assert!(p.contains("- first"));
        assert!(p.contains("- second"));
    }

    #[test]
    fn long_term_renders_under_preferences_and_knowledge() {
        // The heading text differs from `EngineContext`'s
        // ("## User Preferences"), which is exactly why this
        // module exists — the two headings must not silently
        // agree.
        let mut c = Context::default();
        c.memory_context.long_term = vec![entry("I prefer tabs")];
        let p = c.to_prompt();
        assert!(p.contains("## User Preferences & Knowledge"));
        assert!(p.contains("I prefer tabs"));
    }

    #[test]
    fn skills_render_under_relevant_skills() {
        let mut c = Context::default();
        c.skills = vec![skill("rust-refactor", "Do it well.")];
        let p = c.to_prompt();
        assert!(p.contains("## Relevant Skills"));
        assert!(p.contains("### rust-refactor"));
        assert!(p.contains("Do it well."));
    }

    #[test]
    fn system_prompt_renders_after_skills_before_request() {
        // `Context`'s ordering: memory, skills, system, request.
        // `EngineContext` puts system first; the two orderings are
        // a deliberate difference (the memory crate is used by the
        // retrieval path, not the identity path). A regression
        // that moved the system block to the top would blur them.
        let mut c = Context::default();
        c.skills = vec![skill("a", "body")];
        c.system_prompt = Some("You are kod.".to_string());
        c.user_input = "hello".to_string();
        let p = c.to_prompt();
        let skills_pos = p.find("## Relevant Skills").unwrap();
        let sys_pos = p.find("## System").unwrap();
        let req_pos = p.find("## User Request").unwrap();
        assert!(skills_pos < sys_pos, "skills should precede system");
        assert!(sys_pos < req_pos, "system should precede request");
    }

    #[test]
    fn estimated_tokens_scales_with_content() {
        let small = Context {
            user_input: "a".to_string(),
            ..Default::default()
        };
        let big = Context {
            user_input: "x".repeat(4_000),
            ..Default::default()
        };
        assert!(big.estimated_tokens() > small.estimated_tokens());
        // 4-chars-per-token rule of thumb.
        assert!(big.estimated_tokens() >= 900);
        assert!(big.estimated_tokens() <= 1100);
    }

    #[test]
    fn estimated_tokens_counts_memory_and_skills() {
        let mut with_memory = Context::default();
        with_memory.memory_context.working_memory = vec![entry(&"x".repeat(400))];
        let mut with_skill = Context::default();
        with_skill.skills = vec![skill("a", &"y".repeat(400))];
        // Both must contribute; a regression that dropped one
        // would under-report the prompt size.
        assert!(with_memory.estimated_tokens() >= 100);
        assert!(with_skill.estimated_tokens() >= 100);
    }

    #[test]
    fn builder_populates_every_field() {
        let c = ContextBuilder::new()
            .with_user_input("u")
            .with_system_prompt("sys")
            .with_skills(vec![skill("a", "body")])
            .build();
        assert_eq!(c.user_input, "u");
        assert_eq!(c.system_prompt.as_deref(), Some("sys"));
        assert_eq!(c.skills.len(), 1);
    }

    #[test]
    fn builder_with_memory_replaces_the_context() {
        use kod_types::MemoryContext;
        let mut mem = MemoryContext::default();
        mem.long_term = vec![entry("remembered")];
        let c = ContextBuilder::new().with_memory(mem).build();
        assert_eq!(c.memory_context.long_term.len(), 1);
    }
}
