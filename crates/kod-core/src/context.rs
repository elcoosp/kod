//! Engine context - holds all context for processing a request.
//!
//! Combines user input, memory context, and skills into a structured
//! context that can be serialized into a prompt for the LLM.

use kod_types::{MemoryContext, Skill};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Full context for engine processing
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EngineContext {
    pub user_input: String,
    pub memory_context: MemoryContext,
    pub skills: Vec<Skill>,
    pub working_dir: PathBuf,
    pub system_prompt: Option<String>,
    pub tools_available: Vec<String>,
}

impl EngineContext {
    /// Create a new context with user input
    pub fn new(user_input: impl Into<String>) -> Self {
        Self {
            user_input: user_input.into(),
            memory_context: MemoryContext::default(),
            skills: Vec::new(),
            working_dir: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            system_prompt: None,
            tools_available: Vec::new(),
        }
    }

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
            + self.skills.iter().map(|s| s.content.len()).sum::<usize>();

        // Rough: 1 token ≈ 4 characters
        total_chars / 4
    }

    /// Convert to a prompt string for the LLM
    pub fn to_prompt(&self) -> String {
        let mut prompt = String::new();

        // Add system prompt if present
        if let Some(system) = &self.system_prompt {
            prompt.push_str(&format!("## System\n\n{}\n\n", system));
        }

        // Add memory context
        if !self.memory_context.working_memory.is_empty() {
            prompt.push_str("## Current Context\n\n");
            for entry in &self.memory_context.working_memory {
                prompt.push_str(&format!("- {}\n", entry.content));
            }
            prompt.push('\n');
        }

        if !self.memory_context.long_term.is_empty() {
            prompt.push_str("## User Preferences\n\n");
            for entry in &self.memory_context.long_term {
                prompt.push_str(&format!("- {}\n", entry.content));
            }
            prompt.push('\n');
        }

        // Add skills
        if !self.skills.is_empty() {
            prompt.push_str("## Relevant Skills\n\n");
            for skill in &self.skills {
                prompt.push_str(&format!(
                    "### {}\n\n{}\n\n",
                    skill.metadata.name, skill.instructions
                ));
            }
        }

        // Add available tools
        if !self.tools_available.is_empty() {
            prompt.push_str("## Available Tools\n\n");
            for tool in &self.tools_available {
                prompt.push_str(&format!("- {}\n", tool));
            }
            prompt.push('\n');
        }

        // Add user input
        prompt.push_str(&format!("## User Request\n\n{}", self.user_input));

        prompt
    }
}

/// Builder for EngineContext
#[derive(Debug, Default)]
pub struct EngineContextBuilder {
    context: EngineContext,
}

impl EngineContextBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Set user input
    pub fn with_user_input(mut self, input: impl Into<String>) -> Self {
        self.context.user_input = input.into();
        self
    }

    /// Set memory context
    pub fn with_memory(mut self, memory: MemoryContext) -> Self {
        self.context.memory_context = memory;
        self
    }

    /// Set working directory
    pub fn with_working_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.context.working_dir = dir.into();
        self
    }

    /// Set system prompt
    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.context.system_prompt = Some(prompt.into());
        self
    }

    /// Add skills
    pub fn with_skills(mut self, skills: Vec<Skill>) -> Self {
        self.context.skills = skills;
        self
    }

    /// Set available tools
    pub fn with_tools(mut self, tools: Vec<String>) -> Self {
        self.context.tools_available = tools;
        self
    }

    /// Build the context
    pub fn build(self) -> EngineContext {
        self.context
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_context_builder() {
        let context = EngineContextBuilder::new().with_user_input("Test").build();

        assert_eq!(context.user_input, "Test");
    }

    #[test]
    fn test_prompt_generation() {
        let context = EngineContext::new("Help me");
        let prompt = context.to_prompt();

        assert!(prompt.contains("Help me"));
        assert!(prompt.contains("## User Request"));
    }
}

#[cfg(test)]
mod coverage_engine_context {
    //! `EngineContext::to_prompt` produces the historical
    //! text-prompt shape the pre-AD-01 engine handed to the
    //! provider. That shape still ships as the legacy text path
    //! and via `CompletionRequest::render_text`, so a regression
    //! here changes the exact bytes the golden-prefix tests pin.
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
    fn new_sets_the_user_input() {
        let c = EngineContext::new("hello");
        assert_eq!(c.user_input, "hello");
        assert!(c.skills.is_empty());
        assert!(c.tools_available.is_empty());
        assert!(c.system_prompt.is_none());
    }

    #[test]
    fn to_prompt_includes_only_populated_sections() {
        // An empty context has no memory, no skills, no tools and
        // no system prompt. The resulting prompt is the user
        // request section and nothing else — a regression that
        // emitted empty headers would burn tokens on every call.
        let c = EngineContext::new("hi");
        let p = c.to_prompt();
        assert!(p.contains("## User Request"));
        assert!(p.contains("hi"));
        assert!(!p.contains("## Current Context"));
        assert!(!p.contains("## User Preferences"));
        assert!(!p.contains("## Relevant Skills"));
        assert!(!p.contains("## Available Tools"));
        assert!(!p.contains("## System"));
    }

    #[test]
    fn to_prompt_lists_working_memory_entries() {
        let mut c = EngineContext::new("hi");
        c.memory_context.working_memory = vec![entry("first"), entry("second")];
        let p = c.to_prompt();
        assert!(p.contains("## Current Context"));
        assert!(p.contains("- first"));
        assert!(p.contains("- second"));
    }

    #[test]
    fn to_prompt_lists_long_term_entries_under_preferences() {
        let mut c = EngineContext::new("hi");
        c.memory_context.long_term = vec![entry("I prefer tabs")];
        let p = c.to_prompt();
        assert!(p.contains("## User Preferences"));
        assert!(p.contains("I prefer tabs"));
    }

    #[test]
    fn to_prompt_includes_skill_instructions() {
        let mut c = EngineContext::new("hi");
        c.skills = vec![skill("rust-refactor", "Do it well.")];
        let p = c.to_prompt();
        assert!(p.contains("## Relevant Skills"));
        assert!(p.contains("### rust-refactor"));
        assert!(p.contains("Do it well."));
    }

    #[test]
    fn to_prompt_includes_available_tools() {
        let mut c = EngineContext::new("hi");
        c.tools_available = vec!["read_file".to_string(), "write_file".to_string()];
        let p = c.to_prompt();
        assert!(p.contains("## Available Tools"));
        assert!(p.contains("- read_file"));
        assert!(p.contains("- write_file"));
    }

    #[test]
    fn to_prompt_puts_system_prompt_first() {
        // The system prompt is the identity preamble; a regression
        // that moved it below the user request would change the
        // cacheable-prefix boundary.
        let mut c = EngineContext::new("hi");
        c.system_prompt = Some("You are kod.".to_string());
        let p = c.to_prompt();
        let sys_pos = p.find("## System").unwrap();
        let req_pos = p.find("## User Request").unwrap();
        assert!(sys_pos < req_pos, "system section should precede request");
    }

    #[test]
    fn estimated_tokens_scales_with_content_length() {
        let small = EngineContext::new("a");
        let big = EngineContext::new("x".repeat(4000));
        assert!(big.estimated_tokens() > small.estimated_tokens());
        // The 4-chars-per-token rule of thumb: 4000 chars ≈ 1000
        // tokens. The estimate is approximate by design; assert
        // only that the ballpark is preserved.
        assert!(big.estimated_tokens() >= 900);
        assert!(big.estimated_tokens() <= 1100);
    }

    #[test]
    fn builder_populates_every_field() {
        let c = EngineContextBuilder::new()
            .with_user_input("u")
            .with_working_dir("/tmp")
            .with_system_prompt("sys")
            .with_skills(vec![skill("a", "body")])
            .with_tools(vec!["t".to_string()])
            .build();
        assert_eq!(c.user_input, "u");
        assert_eq!(c.working_dir, PathBuf::from("/tmp"));
        assert_eq!(c.system_prompt.as_deref(), Some("sys"));
        assert_eq!(c.skills.len(), 1);
        assert_eq!(c.tools_available, vec!["t".to_string()]);
    }
}
