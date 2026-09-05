//! Task router - classifies and routes tasks to appropriate handlers.
//!
//! The router analyzes user input, classifies it into a task type,
//! builds context from memory/skills, and dispatches to the appropriate handler.

use kod_error::Result;
use kod_memory::manager::MemoryManager;
use kod_skills::matcher::SkillMatcher;
use kod_swarm::swarm::AgentSwarm;
use kod_types::{MemoryContext, ToolCall, ToolResult};
use std::path::PathBuf;
use std::time::Instant;

/// Types of tasks that can be routed
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TaskType {
    Simple,
    CodeModification,
    Debugging,
    Research,
    Testing,
    Documentation,
    Complex,
    MultiStep,
}

/// Configuration for the task router
#[derive(Debug, Clone)]
pub struct RouterConfig {
    pub enable_swarm: bool,
    pub enable_memory: bool,
    pub max_skills_per_query: usize,
    pub working_dir: PathBuf,
}

impl Default for RouterConfig {
    fn default() -> Self {
        Self {
            enable_swarm: true,
            enable_memory: true,
            max_skills_per_query: 3,
            working_dir: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        }
    }
}

/// Response from task processing
#[derive(Debug, Clone)]
pub struct TaskResponse {
    pub task_type: TaskType,
    pub text: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub tool_results: Vec<ToolResult>,
    pub skills_used: Vec<String>,
    pub memory_used: bool,
    pub swarm_used: bool,
    pub execution_time_ms: u64,
    pub usage: Option<kod_provider::TokenUsage>,
}

/// Main task router that coordinates all subsystems
pub struct TaskRouter {
    config: RouterConfig,
    #[allow(dead_code)]
    memory_manager: Option<MemoryManager>,
    skill_matcher: Option<SkillMatcher>,
    swarm: Option<AgentSwarm>,
}

impl TaskRouter {
    /// Create a new task router
    pub fn new(config: RouterConfig, db_path: PathBuf) -> Result<Self> {
        let memory_manager = if config.enable_memory {
            Some(MemoryManager::new(db_path, 100)?)
        } else {
            None
        };

        let skill_matcher = Some(SkillMatcher::new());

        let swarm = if config.enable_swarm {
            Some(AgentSwarm::new(config.working_dir.clone()))
        } else {
            None
        };

        Ok(Self {
            config,
            memory_manager,
            skill_matcher,
            swarm,
        })
    }

    /// Load skills from a directory (matcher uses interior mutability,
    /// so this works through the shared `Arc` in the engine).
    pub async fn load_skills(&self, skills_dir: &std::path::Path) -> Result<usize> {
        let mut loader = kod_skills::loader::SkillLoader::new(skills_dir);
        let skills = loader.load_all().await?;
        let count = skills.len();

        if let Some(matcher) = &self.skill_matcher {
            for skill in skills {
                matcher.add_skill(skill).await;
            }
        }

        Ok(count)
    }

    /// Names of all loaded skills (for `/skills` listing).
    pub async fn loaded_skill_names(&self) -> Vec<String> {
        match &self.skill_matcher {
            Some(matcher) => matcher.skill_names().await,
            None => Vec::new(),
        }
    }

    /// Names + descriptions of all loaded skills (for `/skills` listing).
    pub async fn loaded_skill_details(&self) -> Vec<(String, String)> {
        match &self.skill_matcher {
            Some(matcher) => matcher.skill_details().await,
            None => Vec::new(),
        }
    }

    /// Classify a task based on its content
    pub async fn classify_task(&self, input: &str) -> Result<TaskType> {
        let input_lower = input.to_lowercase();

        // Complex task detection (checked early to catch broad planning terms)
        if input_lower.contains("design")
            || input_lower.contains("architect")
            || input_lower.contains("implement")
            || input_lower.contains("create")
            || input_lower.contains("build")
            || input_lower.contains("complete")
            || input_lower.contains("analyze")
        {
            return Ok(TaskType::Complex);
        }

        // Debugging detection
        if input_lower.contains("debug")
            || input_lower.contains("error")
            || input_lower.contains("traceback")
            || input_lower.contains("panic")
            || input_lower.contains("exception")
        {
            return Ok(TaskType::Debugging);
        }

        // Code modification detection
        if input_lower.contains("refactor")
            || input_lower.contains("fix")
            || input_lower.contains("rename")
            || input_lower.contains("move")
            || input_lower.contains("extract")
            || input_lower.contains("inline")
            || input_lower.contains("modify")
            || input_lower.contains("update")
        {
            return Ok(TaskType::CodeModification);
        }

        // Testing detection
        if input_lower.contains("test") || input_lower.contains("verify") {
            return Ok(TaskType::Testing);
        }

        // Research detection
        if input_lower.contains("research")
            || input_lower.contains("find")
            || input_lower.contains("search")
            || input_lower.contains("look up")
            || input_lower.contains("investigate")
        {
            return Ok(TaskType::Research);
        }

        // Documentation detection
        if input_lower.contains("document")
            || input_lower.contains("docs")
            || input_lower.contains("readme")
            || input_lower.contains("comment")
        {
            return Ok(TaskType::Documentation);
        }

        // Default to simple
        Ok(TaskType::Simple)
    }

    /// Process user input
    pub async fn process_input(&self, input: &str) -> Result<TaskResponse> {
        self.process_input_with_context(input, None).await
    }

    /// Process user input with optional memory context
    pub async fn process_input_with_context(
        &self,
        input: &str,
        memory_context: Option<MemoryContext>,
    ) -> Result<TaskResponse> {
        let start_time = Instant::now();

        // 1. Classify the task
        let task_type = self.classify_task(input).await?;

        // 2. Build context (placeholder - would integrate with memory_manager)
        let _context = self
            .build_context(input, &memory_context, &task_type)
            .await?;

        // 3. Find relevant skills
        let skills_used = self.find_relevant_skills(input).await?;

        // 4. Route to appropriate handler
        let response = match task_type {
            TaskType::Simple => self.handle_simple(input).await?,
            TaskType::CodeModification => self.handle_code_modification(input).await?,
            TaskType::Debugging => self.handle_debugging(input).await?,
            TaskType::Research => self.handle_research(input).await?,
            TaskType::Testing => self.handle_testing(input).await?,
            TaskType::Documentation => self.handle_documentation(input).await?,
            TaskType::Complex | TaskType::MultiStep => self.handle_complex(input).await?,
        };

        // 5. Record execution time
        let execution_time_ms = start_time.elapsed().as_millis() as u64;

        Ok(TaskResponse {
            task_type,
            text: response.text,
            tool_calls: response.tool_calls,
            tool_results: response.tool_results,
            skills_used,
            memory_used: memory_context.is_some(),
            swarm_used: matches!(task_type, TaskType::Complex | TaskType::MultiStep)
                && self.swarm.is_some(),
            execution_time_ms,
            usage: None,
        })
    }

    /// Build context for processing
    async fn build_context(
        &self,
        _input: &str,
        memory_context: &Option<MemoryContext>,
        task_type: &TaskType,
    ) -> Result<String> {
        let mut context = String::new();

        // Add memory context if available
        if let Some(mem_ctx) = memory_context {
            if !mem_ctx.working_memory.is_empty() {
                context.push_str("## Current Context\n\n");
                for entry in &mem_ctx.working_memory {
                    context.push_str(&format!("- {}\n", entry.content));
                }
                context.push('\n');
            }

            if !mem_ctx.long_term.is_empty() {
                context.push_str("## User Preferences\n\n");
                for entry in &mem_ctx.long_term {
                    context.push_str(&format!("- {}\n", entry.content));
                }
                context.push('\n');
            }
        }

        // Add task-specific context
        match task_type {
            TaskType::CodeModification => {
                context.push_str("## Task Type: Code Modification\n\n");
                context.push_str("You are helping with code modification. Analyze the code and propose changes.\n\n");
            }
            TaskType::Debugging => {
                context.push_str("## Task Type: Debugging\n\n");
                context.push_str("You are helping debug an issue. Analyze the error and find the root cause.\n\n");
            }
            TaskType::Research => {
                context.push_str("## Task Type: Research\n\n");
                context.push_str("You are helping research a topic. Find relevant information and summarize.\n\n");
            }
            TaskType::Testing => {
                context.push_str("## Task Type: Testing\n\n");
                context.push_str(
                    "You are helping write tests. Generate comprehensive test cases.\n\n",
                );
            }
            TaskType::Documentation => {
                context.push_str("## Task Type: Documentation\n\n");
                context.push_str(
                    "You are helping write documentation. Create clear and concise docs.\n\n",
                );
            }
            TaskType::Complex | TaskType::MultiStep => {
                context.push_str("## Task Type: Complex Task\n\n");
                context.push_str("This is a complex task that may require multiple steps. Break it down and coordinate.\n\n");
            }
            TaskType::Simple => {
                // No additional context for simple tasks
            }
        }

        Ok(context)
    }

    /// Build a full prompt for LLM generation. `history` is the rendered
    /// transcript of past turns (`(start of conversation)` on the first
    /// turn) — without it every prompt arrives context-free and the model
    /// opens with "this is a fresh conversation".
    pub async fn build_prompt(
        &self,
        input: &str,
        task_type: &TaskType,
        history: &str,
    ) -> Result<String> {
        let mut prompt = String::from(
            "## Identity\n\nYou are kod, a helpful AI assistant running inside the user's machine. \
             You have filesystem tools (function calls, listed under ## Tool use) and a library of \
             skills (## Available skills). When asked what you can do or which skills you have, \
             answer from those lists by name — never invent tool or skill names. \
             Prefer calling tools over guessing, and summarize results in plain text.\n\n",
        );
        prompt.push_str(&self.build_context(input, &None, task_type).await?);

        // Skill knowledge, two layers: the full name+description inventory is
        // always present (so "which skills do you have?" is answerable), and
        // the top relevant skills add their full instructions.
        if let Some(matcher) = &self.skill_matcher {
            let all = matcher.get_all_skills().await;
            if !all.is_empty() {
                let mut names: Vec<(&str, &str)> = all
                    .iter()
                    .map(|s| (s.metadata.name.as_str(), s.metadata.description.as_str()))
                    .collect();
                names.sort();
                const MAX_INVENTORY: usize = 60;
                let listed: Vec<String> = names
                    .iter()
                    .take(MAX_INVENTORY)
                    .map(|(n, d)| format!("- {}: {}", n, d))
                    .collect();
                let more = if names.len() > MAX_INVENTORY {
                    format!(
                        "\n…and {} more (ask /skills for the full list)",
                        names.len() - MAX_INVENTORY
                    )
                } else {
                    String::new()
                };
                prompt.push_str(&format!(
                    "## Available skills ({})\n\n{}{}\n\n",
                    names.len(),
                    listed.join("\n"),
                    more
                ));
            }
            let matches = matcher.find_relevant_skills(input).await;
            if !matches.is_empty() {
                prompt.push_str("## Relevant Skills\n\n");
                for skill_match in matches.iter().take(self.config.max_skills_per_query) {
                    prompt.push_str(&format!(
                        "### {}\n\n{}\n\n",
                        skill_match.skill.metadata.name, skill_match.skill.instructions
                    ));
                }
            }
        }

        // Add user input
        prompt.push_str(&format!(
            "## Conversation so far\n\n{}\n\n## User Request\n\n{}",
            history, input
        ));

        Ok(prompt)
    }
    async fn find_relevant_skills(&self, input: &str) -> Result<Vec<String>> {
        if let Some(matcher) = &self.skill_matcher {
            let matches = matcher.find_relevant_skills(input).await;
            Ok(matches
                .iter()
                .take(self.config.max_skills_per_query)
                .map(|m| m.skill.metadata.name.clone())
                .collect())
        } else {
            Ok(Vec::new())
        }
    }

    /// Handle simple tasks (direct LLM call)
    async fn handle_simple(&self, input: &str) -> Result<HandlerResponse> {
        Ok(HandlerResponse {
            text: Some(format!("Processing simple task: {}", input)),
            tool_calls: Vec::new(),
            tool_results: Vec::new(),
        })
    }

    /// Handle code modification tasks
    async fn handle_code_modification(&self, input: &str) -> Result<HandlerResponse> {
        Ok(HandlerResponse {
            text: Some(format!("Processing code modification: {}", input)),
            tool_calls: Vec::new(),
            tool_results: Vec::new(),
        })
    }

    /// Handle debugging tasks
    async fn handle_debugging(&self, input: &str) -> Result<HandlerResponse> {
        Ok(HandlerResponse {
            text: Some(format!("Processing debugging task: {}", input)),
            tool_calls: Vec::new(),
            tool_results: Vec::new(),
        })
    }

    /// Handle research tasks
    async fn handle_research(&self, input: &str) -> Result<HandlerResponse> {
        Ok(HandlerResponse {
            text: Some(format!("Processing research task: {}", input)),
            tool_calls: Vec::new(),
            tool_results: Vec::new(),
        })
    }

    /// Handle testing tasks
    async fn handle_testing(&self, input: &str) -> Result<HandlerResponse> {
        Ok(HandlerResponse {
            text: Some(format!("Processing testing task: {}", input)),
            tool_calls: Vec::new(),
            tool_results: Vec::new(),
        })
    }

    /// Handle documentation tasks
    async fn handle_documentation(&self, input: &str) -> Result<HandlerResponse> {
        Ok(HandlerResponse {
            text: Some(format!("Processing documentation task: {}", input)),
            tool_calls: Vec::new(),
            tool_results: Vec::new(),
        })
    }

    /// Handle complex tasks (may use swarm)
    async fn handle_complex(&self, input: &str) -> Result<HandlerResponse> {
        if self.swarm.is_some() {
            // Use swarm coordination
            // In a full implementation, this would delegate to the swarm
            Ok(HandlerResponse {
                text: Some(format!(
                    "Complex task received for swarm coordination: {}",
                    input
                )),
                tool_calls: Vec::new(),
                tool_results: Vec::new(),
            })
        } else {
            // Fallback to simple processing
            self.handle_simple(input).await
        }
    }
}

/// Internal response from task handlers
#[derive(Debug, Clone)]
struct HandlerResponse {
    text: Option<String>,
    tool_calls: Vec<ToolCall>,
    tool_results: Vec<ToolResult>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_task_classification() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");

        let router = TaskRouter::new(RouterConfig::default(), db_path).unwrap();

        assert_eq!(
            router.classify_task("What is 2+2?").await.unwrap(),
            TaskType::Simple
        );
        assert_eq!(
            router.classify_task("Fix the bug").await.unwrap(),
            TaskType::CodeModification
        );
        assert_eq!(
            router.classify_task("Debug this error").await.unwrap(),
            TaskType::Debugging
        );
        assert_eq!(
            router
                .classify_task("Research async patterns")
                .await
                .unwrap(),
            TaskType::Research
        );
    }

    #[tokio::test]
    async fn test_build_prompt_carries_identity_and_skill_inventory() {
        use kod_types::{Skill, SkillId, SkillMetadata};

        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");

        let router = TaskRouter::new(RouterConfig::default(), db_path).unwrap();
        let task_type = router
            .classify_task("which skills do you have?")
            .await
            .unwrap();

        // No skills loaded: identity + request still present.
        let bare = router
            .build_prompt("hello?", &task_type, "(start of conversation)")
            .await
            .unwrap();
        assert!(bare.contains("You are kod"), "identity preamble missing");
        assert!(bare.contains("hello?"), "user request missing");
        assert!(
            bare.contains("## Conversation so far"),
            "history section missing"
        );

        // One loaded skill: its name + description join the inventory.
        let matcher = router.skill_matcher.as_ref().expect("matcher present");
        matcher
            .add_skill(Skill {
                id: SkillId::new(),
                metadata: SkillMetadata {
                    name: "ui-ux-designer".to_string(),
                    description: "Design help".to_string(),
                    version: "1.0.0".to_string(),
                    author: None,
                    category: "test".to_string(),
                    tags: Vec::new(),
                    capabilities: Vec::new(),
                    requirements: Vec::new(),
                    triggers: Vec::new(),
                },
                instructions: String::new(),
                examples: Vec::new(),
                constraints: None,
                content: String::new(),
                path: temp_dir.path().to_path_buf(),
            })
            .await;
        let with_skill = router
            .build_prompt(
                "which skills do you have?",
                &task_type,
                "User: what can you do?\nAssistant: I can help.\n",
            )
            .await
            .unwrap();
        assert!(
            with_skill.contains("ui-ux-designer") && with_skill.contains("Design help"),
            "skill inventory missing: {with_skill}"
        );
        assert!(
            with_skill.contains("what can you do?"),
            "history not carried: {with_skill}"
        );
    }
}
