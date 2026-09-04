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
            + self.skills.iter().map(|s| s.content.len()).sum::<usize>();

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
