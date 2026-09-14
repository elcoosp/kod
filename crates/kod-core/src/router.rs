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

/// Truncate a UTF-8 string to at most `max` bytes at a char boundary.
/// Local to this module so the router does not need a public dependency
/// on `kod_core::engine`'s helper.
fn truncate_chars(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

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
    /// The model's context window, in tokens. Used to size the memory
    /// manager's per-prompt context budget so a long-running session
    /// does not silently drop memory entries that would comfortably fit
    /// a large model. Callers that build the router from `LlmConfig`
    /// should pass `llm.context_window`. Defaults to `8192` — the same
    /// value `LlmConfig::default()` uses — so a caller that ignores
    /// the field still gets a sane memory budget rather than the
    /// `MemoryManager`'s own 4096 hardcode.
    pub context_window: usize,
}

impl Default for RouterConfig {
    fn default() -> Self {
        Self {
            enable_swarm: true,
            enable_memory: true,
            max_skills_per_query: 3,
            working_dir: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            context_window: 8192,
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
    /// Names of the skills whose instructions were injected into the
    /// prompt. Empty when no skill matched.
    pub skills_used: Vec<String>,
    /// True iff at least one memory entry was included in the prompt.
    pub memory_used: bool,
    /// True iff the swarm handled part of this task.
    ///
    /// Always `false` today. The swarm is registered in the router
    /// (when `enable_swarm` is set) but `handle_complex` returns a
    /// placeholder string rather than routing the task to any agent —
    /// so nothing has ever been dispatched through the swarm, and the
    /// field cannot honestly be `true`. The field is kept so the
    /// response shape is stable for the swarm-dispatch implementation,
    /// but it does not currently carry a signal.
    ///
    /// The previous computation — `matches!(task_type, Complex) &&
    /// self.swarm.is_some()` — was the same kind of tautology that
    /// `memory_used` used to be: a fact about the router's inputs
    /// (how it classified the task, whether it owns a swarm object)
    /// dressed up as a fact about what happened.
    pub swarm_used: bool,
    /// Wall-clock time from `process_input` entry to response.
    pub execution_time_ms: u64,
    /// Token usage the provider reported, when it did.
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
            // The manager's own default is a fixed 4096-token budget.
            // Set it from the model's actual context window so the
            // retrieve-side cap does not throw away memory entries that
            // would have fit — the failure mode is invisible (memory
            // silently under-populates rather than erroring).
            let mut manager = MemoryManager::new(db_path, 100)?;
            manager.set_context_window(config.context_window.max(1_000));
            Some(manager)
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

    /// Classify a task based on its content.
    ///
    /// Order matters: specific intents (debugging, code-mod, testing,
    /// docs, research) are checked before broad planning verbs (design,
    /// implement, create, build). Without this ordering, a request like
    /// "test the create function" was misclassified as Complex because
    /// "create" matched first; "debug the created function" was likewise
    /// Complex instead of Debugging.
    ///
    /// Matching is whole-word, not substring: "prefix" no longer matches
    /// "fix", "remove" no longer matches "move", "testify" no longer
    /// matches "test".
    pub async fn classify_task(&self, input: &str) -> Result<TaskType> {
        // Case-insensitive whole-word substring test.
        fn contains_word(haystack: &str, needle: &str) -> bool {
            if needle.is_empty() {
                return false;
            }
            let mut start = 0;
            while let Some(rel) = haystack[start..].find(needle) {
                let abs = start + rel;
                let before_ok = abs == 0
                    || !haystack.as_bytes()[abs - 1].is_ascii_alphanumeric();
                let after_idx = abs + needle.len();
                let after_ok = after_idx >= haystack.len()
                    || !haystack.as_bytes()[after_idx].is_ascii_alphanumeric();
                if before_ok && after_ok {
                    return true;
                }
                start = abs + 1;
                if start >= haystack.len() {
                    break;
                }
            }
            false
        }

        let input_lower = input.to_lowercase();

        // Priority order (first match wins):
        //   1. Debugging   -- most specific failure vocabulary
        //   2. CodeMod     -- surgical action verbs
        //   3. Complex     -- broad planning verbs; a "design/build" request
        //                     that also mentions tests is still Complex
        //   4. Research    -- "investigate/find/search"; outranks docs because
        //                     "research the docs" is a research task
        //   5. Testing     -- specific testing verbs
        //   6. Documentation -- "document/readme/comment"; weakest signal
        //                       (comments show up in code snippets)
        //   7. Simple      -- default
        // Whole-word matching (not substring): "prefix" does not match
        // "fix", "remove" does not match "move", "testify" does not match
        // "test".

        // 1. Debugging
        if ["debug", "error", "traceback", "panic", "exception"]
            .iter()
            .copied()
            .any(|w| contains_word(&input_lower, w))
        {
            return Ok(TaskType::Debugging);
        }

        // 2. Code modification
        if [
            "refactor", "fix", "rename", "move", "extract", "inline", "modify", "update",
        ]
        .iter()
        .copied()
        .any(|w| contains_word(&input_lower, w))
        {
            return Ok(TaskType::CodeModification);
        }

        // 3. Complex -- broad planning verbs.
        if [
            "design",
            "architect",
            "implement",
            "create",
            "build",
            "complete",
            "analyze",
        ]
        .iter()
        .copied()
        .any(|w| contains_word(&input_lower, w))
        {
            return Ok(TaskType::Complex);
        }

        // 4. Research
        if ["research", "find", "search", "investigate", "look up"]
            .iter()
            .copied()
            .any(|w| contains_word(&input_lower, w))
        {
            return Ok(TaskType::Research);
        }

        // 5. Testing
        if ["test", "tests", "testing", "verify"]
            .iter()
            .copied()
            .any(|w| contains_word(&input_lower, w))
        {
            return Ok(TaskType::Testing);
        }

        // 6. Documentation
        if [
            "document", "documentation", "docs", "readme", "comment", "comments",
        ]
        .iter()
        .copied()
        .any(|w| contains_word(&input_lower, w))
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

        // Did memory actually contribute to this prompt? The flag used
        // to be `memory_context.is_some()`, which is true whenever the
        // router has a manager — i.e. always, since enable_memory
        // defaults on. The observable meaning to a caller is "at least
        // one memory entry was included", and that is what this
        // reports.
        let memory_used = memory_context
            .as_ref()
            .map(|c| {
                !c.working_memory.is_empty()
                    || !c.long_term.is_empty()
                    || !c.episodic.is_empty()
            })
            .unwrap_or(false);

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
            memory_used,
            // No dispatch path uses the swarm today: `handle_complex`
            // returns a placeholder string, and nothing else consults
            // the router's `swarm` field for work routing. Report the
            // honest answer — the swarm did not handle this task.
            swarm_used: false,
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
             The full instructions of any skill whose name or triggers appear in the user's request \
             are already inserted under ## Relevant Skills. If the user asks for a skill's \
             instructions, answer directly from that context — do not call tools to look it up. \
             Prefer calling tools over guessing, and summarize results in plain text.\n\n",
        );
        // Retrieve memory context. Before this, the router constructed
        // a `MemoryManager` and marked the field `#[allow(dead_code)]`:
        // nothing wrote to memory, nothing read from it, and
        // `build_prompt` passed a hardcoded `&None`. The whole memory
        // layer was inert. Reading here closes half the loop — anything
        // stored in long-term memory (or short-term, if a caller
        // populates it) now reaches the prompt.
        //
        // `retrieve_context` on an empty manager is cheap: a redb
        // substring scan over an empty table and a short-term recency
        // slice, both no-ops for a fresh session.
        let memory_context = match &self.memory_manager {
            Some(manager) => Some(manager.retrieve_context(input).await?),
            None => None,
        };
        prompt.push_str(&self.build_context(input, &memory_context, task_type).await?);

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
                    let skill = &skill_match.skill;
                    // Tell the agent where the skill lives so it can read
                    // reference files with the correct absolute path instead of
                    // guessing relative to the project root.
                    let base_dir = skill
                        .path
                        .parent()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| ".".to_string());
                    prompt.push_str(&format!(
                        "### {}\nSkill location: {}\n\n{}\n\n",
                        skill.metadata.name, base_dir, skill.instructions
                    ));

                    // The parser splits a skill file into instructions,
                    // examples, and constraints. Only the instructions
                    // section was being injected, so a skill whose value
                    // lives in its worked examples (code-review's
                    // <example> blocks, python-testing's parametrize
                    // sample) arrived at the model as bare prose — the
                    // model had no way to reproduce the skill author's
                    // intent. Append the examples and constraints too.
                    //
                    // These are secondary; cap them so a verbose skill
                    // cannot dominate the prompt. The instructions are
                    // the primary content and stay uncapped.
                    const MAX_SKILL_EXAMPLES: usize = 5;
                    const MAX_EXAMPLE_CHARS: usize = 1_500;
                    if !skill.examples.is_empty() {
                        prompt.push_str("Examples:\n\n");
                        for ex in skill.examples.iter().take(MAX_SKILL_EXAMPLES) {
                            if !ex.input.trim().is_empty() {
                                prompt.push_str(&format!("Input: {}\n", ex.input.trim()));
                            }
                            let out = ex.output.trim();
                            let shown = if out.len() > MAX_EXAMPLE_CHARS {
                                format!("{}…", truncate_chars(out, MAX_EXAMPLE_CHARS))
                            } else {
                                out.to_string()
                            };
                            prompt.push_str(&format!("Output:\n{}\n\n", shown));
                        }
                        let extra = skill.examples.len().saturating_sub(MAX_SKILL_EXAMPLES);
                        if extra > 0 {
                            prompt.push_str(&format!(
                                "…and {} more example(s) in the skill file.\n\n",
                                extra
                            ));
                        }
                    }
                    if let Some(constraints) = &skill.constraints
                        && !constraints.trim().is_empty()
                    {
                        prompt.push_str(&format!(
                            "Constraints:\n{}\n\n",
                            constraints.trim()
                        ));
                    }
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

    /// `TaskResponse::memory_used` must be true only when a memory
    /// entry actually reached the prompt. Regression: it was set to
    /// `memory_context.is_some()`, which is true whenever a manager
    /// exists — always, since enable_memory defaults on — so the flag
    /// reported "memory infrastructure present", not "memory used".
    #[tokio::test]
    async fn test_memory_used_flag_reflects_contribution() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");
        let router = TaskRouter::new(
            RouterConfig {
                enable_memory: true,
                enable_swarm: false,
                max_skills_per_query: 3,
                working_dir: temp_dir.path().to_path_buf(),
                context_window: 8192,
            },
            db_path,
        )
        .unwrap();

        // Fresh manager: no entries, so the flag is false even though
        // the manager is present.
        let resp = router
            .process_input("what is the meaning of life?")
            .await
            .unwrap();
        assert!(
            !resp.memory_used,
            "empty memory must not report as used"
        );

        // Add a fact whose content shares a content word with the
        // next prompt, and confirm the flag flips.
        let manager = router.memory_manager.as_ref().unwrap();
        manager
            .store(kod_types::MemoryType::LongTerm, "The project is called KOD.")
            .await
            .unwrap();
        let resp = router
            .process_input("tell me about the project")
            .await
            .unwrap();
        assert!(
            resp.memory_used,
            "a matching memory entry must report as used"
        );

        // A prompt sharing no content word with the stored fact must
        // leave the flag false.
        let resp = router
            .process_input("xyzzy plugh frobnicate")
            .await
            .unwrap();
        assert!(
            !resp.memory_used,
            "non-matching prompt must not report as used"
        );
    }

    /// `build_prompt` must consult the memory manager. Regression:
    /// the router built a MemoryManager and marked the field
    /// `#[allow(dead_code)]`; `build_context` was always called with
    /// `&None`, so anything stored in memory — including long-term
    /// facts written via `MemoryManager::store` — never reached the
    /// prompt.
    #[tokio::test]
    async fn test_build_prompt_includes_memory_context() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");

        // enable_memory on so the router constructs a MemoryManager.
        let router = TaskRouter::new(
            RouterConfig {
                enable_memory: true,
                enable_swarm: false,
                max_skills_per_query: 3,
                working_dir: temp_dir.path().to_path_buf(),
                context_window: 8192,
            },
            db_path,
        )
        .unwrap();

        // Store a durable fact directly through the manager. This is
        // the shape a future "remember this" tool would use.
        let manager = router.memory_manager.as_ref().expect("memory enabled");
        manager
            .store(
                kod_types::MemoryType::LongTerm,
                "The user's project is called KOD.",
            )
            .await
            .unwrap();

        // A prompt whose input contains the search term must pull the
        // fact into the memory context block.
        let prompt = router
            .build_prompt(
                "tell me about the project",
                &TaskType::Simple,
                "(start of conversation)",
            )
            .await
            .unwrap();
        assert!(
            prompt.contains("KOD"),
            "long-term memory entry missing from prompt:\n{prompt}"
        );
    }

    /// RouterConfig::context_window must reach the memory manager so its
    /// token budget is the model's window, not the manager's own 4096
    /// hardcode.
    #[tokio::test]
    async fn test_router_config_context_window_reaches_manager() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");
        let router = TaskRouter::new(
            RouterConfig {
                enable_memory: true,
                enable_swarm: false,
                max_skills_per_query: 3,
                working_dir: temp_dir.path().to_path_buf(),
                context_window: 131_072,
            },
            db_path,
        )
        .unwrap();

        let manager = router.memory_manager.as_ref().unwrap();
        for i in 0..20 {
            manager
                .store(
                    kod_types::MemoryType::LongTerm,
                    &format!("fact-{i}-{}", "x".repeat(400)),
                )
                .await
                .unwrap();
        }
        let ctx = manager.retrieve_context("fact").await.unwrap();
        assert!(
            !ctx.long_term.is_empty(),
            "large window should have kept memory entries"
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
                instructions: "Do design well.".to_string(),
                examples: vec![kod_types::SkillExample {
                    input: "make a login form".to_string(),
                    output: "Use a single column with a labeled email field.".to_string(),
                }],
                constraints: Some("Never use more than two fonts.".to_string()),
                content: String::new(),
                path: temp_dir.path().to_path_buf(),
            })
            .await;
        let with_skill = router
            .build_prompt(
                "which skills do you have — tell me about ui-ux-designer?",
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
            with_skill.contains("Skill location:"),
            "skill location not injected: {with_skill}"
        );
        assert!(
            with_skill.contains("what can you do?"),
            "history not carried: {with_skill}"
        );
        // The examples and constraints the parser extracted must reach
        // the prompt. Regression: build_prompt only injected the
        // instructions section, so skills whose value is in their
        // worked examples arrived at the model as bare prose.
        assert!(
            with_skill.contains("make a login form"),
            "example input missing from prompt: {with_skill}"
        );
        assert!(
            with_skill.contains("labeled email field"),
            "example output missing from prompt: {with_skill}"
        );
        assert!(
            with_skill.contains("Never use more than two fonts"),
            "constraints missing from prompt: {with_skill}"
        );
    }
}
