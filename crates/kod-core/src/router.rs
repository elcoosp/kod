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

    /// Load skills from a directory
    pub async fn load_skills(&mut self, skills_dir: &std::path::Path) -> Result<()> {
        let mut loader = kod_skills::loader::SkillLoader::new(skills_dir);
        let skills = loader.load_all().await?;

        if let Some(matcher) = &self.skill_matcher {
            for skill in skills {
                matcher.add_skill(skill).await;
            }
        }

        Ok(())
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
        let _context = self.build_context(input, &memory_context, &task_type).await?;

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
                context.push_str("You are helping write tests. Generate comprehensive test cases.\n\n");
            }
            TaskType::Documentation => {
                context.push_str("## Task Type: Documentation\n\n");
                context.push_str("You are helping write documentation. Create clear and concise docs.\n\n");
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

    /// Find relevant skills for input
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
                text: Some(format!("Complex task received for swarm coordination: {}", input)),
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

        assert_eq!(router.classify_task("What is 2+2?").await.unwrap(), TaskType::Simple);
        assert_eq!(router.classify_task("Fix the bug").await.unwrap(), TaskType::CodeModification);
        assert_eq!(router.classify_task("Debug this error").await.unwrap(), TaskType::Debugging);
        assert_eq!(router.classify_task("Research async patterns").await.unwrap(), TaskType::Research);
    }
}
