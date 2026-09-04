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
            + self.memory_context.working_memory.iter().map(|m| m.content.len()).sum::<usize>()
            + self.memory_context.long_term.iter().map(|m| m.content.len()).sum::<usize>()
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
                prompt.push_str(&format!("### {}\n\n{}\n\n", skill.metadata.name, skill.instructions));
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
        let context = EngineContextBuilder::new()
            .with_user_input("Test")
            .build();

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
