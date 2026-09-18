//! Task router - classifies and routes tasks to appropriate handlers.
//!
//! The router analyzes user input, classifies it into a task type,
//! builds context from memory/skills, and dispatches to the appropriate handler.

use std::sync::Arc;

use kod_error::Result;
use kod_memory::manager::MemoryManager;
use kod_skills::matcher::SkillMatcher;
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
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
#[derive(Clone)]
pub struct RouterConfig {
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
    /// Short-term memory capacity. Read from `MemoryConfig::short_term_capacity`.
    /// Defaults to 100 for callers that build RouterConfig directly.
    pub short_term_capacity: usize,
    /// Minimum score for a skill to be considered a match. Read from
    /// `SkillsConfig::match_threshold` by the CLI and TUI; the
    /// builder default (0.3) matches the pre-config-plumbing
    /// hardcoded value so a caller that ignores the field sees no
    /// change.
    pub skill_threshold: f32,
    /// Optional embedder for semantic memory retrieval (design D2.1).
    /// `None` (the default) keeps the keyword+recency fallback the D2.3
    /// scorer provides. A caller that wants semantic scoring builds one
    /// from `memory.embedding_endpoint` via
    /// `kod_memory::embedding::from_config` and hands it here.
    ///
    /// The embedder is installed on the `MemoryManager` inside
    /// `TaskRouter::new` — before the router goes behind an `Arc` and
    /// `set_embedder`'s `&mut self` becomes unreachable. The field is
    /// therefore part of the config, not a separate post-construction
    /// setter.
    pub embedder: Option<std::sync::Arc<dyn kod_memory::EmbeddingClient>>,
}

impl std::fmt::Debug for RouterConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `dyn EmbeddingClient` is not `Debug`; the manual impl shows
        // the embedder by name, which is what a log line wants.
        f.debug_struct("RouterConfig")
            .field("enable_memory", &self.enable_memory)
            .field("max_skills_per_query", &self.max_skills_per_query)
            .field("working_dir", &self.working_dir)
            .field("context_window", &self.context_window)
            .field("short_term_capacity", &self.short_term_capacity)
            .field("skill_threshold", &self.skill_threshold)
            .field("embedder", &self.embedder.as_ref().map(|e| e.name()))
            .finish()
    }
}

impl Default for RouterConfig {
    fn default() -> Self {
        Self {
            enable_memory: true,
            max_skills_per_query: 3,
            working_dir: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            context_window: 8192,
            short_term_capacity: 100,
            skill_threshold: 0.3,
            embedder: None,
        }
    }
}

/// Response from task processing
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TaskResponse {
    pub task_type: TaskType,
    /// Model-generated reply text. `None` when produced by
    /// [`TaskRouter::process_input`] — the router classifies and
    /// describes, it does not generate. The engine fills this with
    /// the provider's reply.
    pub text: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub tool_results: Vec<ToolResult>,
    /// Names of the skills whose instructions were injected into the
    /// prompt. Empty when no skill matched.
    pub skills_used: Vec<String>,
    /// True iff at least one memory entry was included in the prompt.
    pub memory_used: bool,
    /// Wall-clock time from `process_input` entry to response.
    pub execution_time_ms: u64,
    /// Token usage the provider reported, when it did.
    #[serde(default)]
    pub usage: Option<kod_provider::TokenUsage>,
    /// The memory context that was retrieved for this prompt. Carried in
    /// the response so the engine can pass it to
    /// [`TaskRouter::build_prompt_with_context`] without a second redb
    /// scan — the single-retrieval path (D0.4). `None` when memory is
    /// disabled or the caller supplied a context directly.
    #[serde(default)]
    pub memory_context: Option<MemoryContext>,
    /// USD pricing of the endpoint that served this response, when
    /// the endpoint has a `[pricing]` block. `None` for a local
    /// endpoint (no cost), a remote endpoint without a pricing
    /// block, or a response that never reached a provider.
    ///
    /// Carried on the response rather than looked up from the
    /// engine at display time because the model that served the
    /// call is known only inside `process_*`, after the fallback
    /// chain resolves. A later lookup would report the *current*
    /// model's pricing, which after a `/model` switch is not the
    /// one that produced the tokens.
    #[serde(default)]
    pub pricing: Option<kod_provider::ModelPricing>,
}

/// The structured form of a router prompt (design §2 AD-16).
///
/// A `PromptPlan` is what a provider's `complete` method wants:
///
/// - a system prompt split into cacheable and volatile segments, in
///   order, so a provider with explicit cache support (Anthropic's
///   `cache_control`) can place a breakpoint on the last cacheable one,
///   and a provider with implicit prefix caching (OpenAI-compatible
///   servers, LM Studio, vLLM) can rely on the byte-stability of the
///   concatenation.
/// - a rendered transcript block — the historical turns plus the user's
///   current request — which is always volatile.
///
/// The concrete split point is the `## Volatile suffix` marker the
/// router already emits between its stable block (Identity + RepoMap)
/// and its volatile one (Environment + Tool inventory + skills +
/// history + user request). That marker was a text convention; this
/// struct makes it a type. `render_text` reproduces the pre-`PromptPlan`
/// prompt byte for byte, which is the invariant the migration asserts.
///
/// The plan is produced by [`TaskRouter::build_prompt_plan`]. Until the
/// engine migrates to `CompletionRequest` (AD-01), the plan is available
/// for callers and tests but the engine still calls
/// `build_prompt_with_budget` directly — `build_prompt_plan` internally
/// calls it and splits the result, so the two paths cannot drift.
#[derive(Debug, Clone)]
pub struct PromptPlan {
    /// Ordered segments. `cacheable == true` for the invariant prefix
    /// (Identity + RepoMap) and `false` for everything the model sees
    /// that depends on the turn — the environment block, the tool
    /// inventory, the skills, the transcript, and the user's request.
    pub system: Vec<SystemSegment>,
    /// The rendered transcript + user request. Always volatile.
    pub messages_text: String,
}

/// One ordered slice of the system prompt. See [`PromptPlan`].
#[derive(Debug, Clone)]
pub struct SystemSegment {
    pub text: String,
    /// `true` when this segment is part of the byte-stable invariant
    /// prefix across turns of a session. A provider with explicit cache
    /// support places a breakpoint at the **last** cacheable segment.
    pub cacheable: bool,
}

impl PromptPlan {
    /// Split a rendered prompt at the design's marker. Everything
    /// strictly before `## Volatile suffix` is a single cacheable
    /// segment; everything from that marker on is a single volatile
    /// segment. A prompt without the marker is treated as entirely
    /// volatile (a degraded but valid state — a caller that renders a
    /// custom prompt without the router's convention loses the cache
    /// benefit rather than failing).
    ///
    /// One segment per region, not one per line: the wire does not
    /// care how the byte sequence is chunked, only that the cacheable
    /// prefix is byte-stable. Splitting per section would multiply
    /// breakpoints for no benefit.
    pub fn from_rendered(rendered: &str) -> Self {
        const VOLATILE_MARKER: &str = "## Volatile suffix";
        match rendered.find(VOLATILE_MARKER) {
            Some(idx) => {
                let (stable, volatile) = rendered.split_at(idx);
                Self {
                    system: vec![
                        SystemSegment {
                            text: stable.to_string(),
                            cacheable: true,
                        },
                        SystemSegment {
                            text: volatile.to_string(),
                            cacheable: false,
                        },
                    ],
                    messages_text: String::new(),
                }
            }
            None => Self {
                system: vec![SystemSegment {
                    text: rendered.to_string(),
                    cacheable: false,
                }],
                messages_text: String::new(),
            },
        }
    }

    /// Reconstruct the text this plan was built from. Byte-identical to
    /// the pre-`PromptPlan` prompt — that equivalence is the invariant
    /// the `prompt_plan_renders_identically` test asserts.
    pub fn render_text(&self) -> String {
        let mut out = String::new();
        for seg in &self.system {
            out.push_str(&seg.text);
        }
        out.push_str(&self.messages_text);
        out
    }

    /// The concatenation of every cacheable segment, in order. This is
    /// the string a provider with explicit cache support places a
    /// breakpoint after; the golden-prefix test asserts it is stable
    /// across turns in a session.
    pub fn cacheable_prefix(&self) -> String {
        let mut out = String::new();
        for seg in &self.system {
            if seg.cacheable {
                out.push_str(&seg.text);
            }
        }
        out
    }
}

/// Main task router that coordinates all subsystems
pub struct TaskRouter {
    config: RouterConfig,
    memory_manager: Option<MemoryManager>,
    /// The live skill set. `Arc` so the hot-reload task can hold a
    /// `Weak` reference and the matcher can be shared without
    /// duplicating its interior state.
    skill_matcher: Option<Arc<SkillMatcher>>,
    /// Watchers kept alive for the router's lifetime — dropping a
    /// `SkillWatcher` stops the OS watch and the background task.
    /// One per directory that has hot reload enabled; a `Mutex`
    /// because the router is behind an `Arc` and enabling happens
    /// through `&self`.
    skill_watchers: std::sync::Mutex<Vec<kod_skills::SkillWatcher>>,
    /// Repository map cache with mtime-based invalidation. Stores the
    /// structured `RepoMap` (not the pre-rendered string) so future
    /// consumers — PageRank in D5, a `/map` command that wants counts —
    /// can reuse the same build without another walk.
    ///
    /// Invalidation is checked at the start of every `build_prompt`
    /// call, never mid-turn: the cacheable prefix of a single prompt
    /// must stay byte-identical from the first provider call to the
    /// last. This is the "prompt cache is a first-class resource"
    /// principle (principle n°2 of the roadmap).
    repo_map_cache: crate::router::RepoMapCache,
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
            let mut manager = MemoryManager::new(db_path, config.short_term_capacity.max(1))?;
            manager.set_context_window(config.context_window.max(1_000));
            // Design D2.1: install the embedder the caller built from
            // `memory.embedding_endpoint`. Done here, before the router
            // is behind an `Arc`, because `MemoryManager::set_embedder`
            // takes `&mut self` and the Arc makes that unreachable
            // afterwards.
            if let Some(embedder) = config.embedder.clone() {
                tracing::info!(
                    name = embedder.name(),
                    dims = embedder.dims(),
                    "memory embedder installed",
                );
                manager.set_embedder(embedder);
            }
            Some(manager)
        } else {
            None
        };

        let skill_matcher = Some(Arc::new(SkillMatcher::with_threshold(
            config.skill_threshold,
        )));

        Ok(Self {
            config,
            memory_manager,
            skill_matcher,
            skill_watchers: std::sync::Mutex::new(Vec::new()),
            repo_map_cache: crate::router::RepoMapCache::new(),
        })
    }

    /// Store one turn's text in short-term memory.
    ///
    /// The engine calls this after recording a turn, so the working
    /// set the next prompt retrieves through `retrieve_context`
    /// reflects the current session. Short-term is the right layer:
    /// it is FIFO-bounded, evicts at capacity, and needs no fact
    /// extraction to be useful.
    ///
    /// Not written to long-term or episodic:
    ///   - Long-term holds durable facts. Deciding what deserves to
    ///     persist requires an extraction pass (an LLM call) that
    ///     this method does not do. Auto-writing every turn would
    ///     make the long-term store indistinguishable from the
    ///     transcript it is supposed to summarise.
    ///   - Episodic holds embeddings. The manager writes them empty
    ///     today (fastembed is not wired), so a write there would be
    ///     inert. It gains a write path with the embedding work.
    ///
    /// Best-effort: a store failure logs a warning and returns
    /// `Ok(())` — losing a session turn from the working set is not a
    /// reason to fail a completed prompt. A manager that is not
    /// configured (`enable_memory: false`) is a silent no-op, which
    /// is what a test that disabled memory expects.
    pub async fn store_short_term(&self, content: &str) -> Result<()> {
        let Some(manager) = &self.memory_manager else {
            return Ok(());
        };
        if content.trim().is_empty() {
            return Ok(());
        }
        if let Err(e) = manager
            .store(kod_types::MemoryType::ShortTerm, content)
            .await
        {
            tracing::warn!(
                error = %e,
                "could not store turn in short-term memory"
            );
        }
        Ok(())
    }

    /// Store an episodic memory entry with caller-supplied metadata
    /// (design D2.5). Used by `KodEngine::extract_memories_now`: the
    /// auto-extracted facts carry a `session_id` and the `auto-*` tags
    /// the extractor produced, and they are stored as `Episodic` so the
    /// consolidation pass can archive them on age without touching the
    /// durable `LongTerm` layer.
    ///
    /// Errors when memory is disabled, matching `store_long_term`.
    pub async fn store_episodic(
        &self,
        content: &str,
        metadata: kod_types::MemoryMetadata,
    ) -> Result<kod_types::MemoryId> {
        let Some(manager) = &self.memory_manager else {
            return Err(kod_error::KodError::Config(
                "memory is disabled in this session (RouterConfig::enable_memory = false)"
                    .to_string(),
            ));
        };
        manager
            .store_with_metadata(kod_types::MemoryType::Episodic, content, metadata)
            .await
    }

    /// Store a long-term memory entry with a project scope and tags
    /// (D2-B3a). Called by the `memory_save` tool. Returns the new
    /// entry's id. Errors when memory is disabled.
    pub async fn store_long_term(
        &self,
        content: &str,
        tags: Vec<String>,
        project_key: Option<String>,
    ) -> Result<kod_types::MemoryId> {
        let Some(manager) = &self.memory_manager else {
            return Err(kod_error::KodError::Config(
                "memory is disabled in this session (RouterConfig::enable_memory = false)"
                    .to_string(),
            ));
        };
        let metadata = kod_types::MemoryMetadata {
            tags,
            project_key,
            ..Default::default()
        };
        manager
            .store_with_metadata(kod_types::MemoryType::LongTerm, content, metadata)
            .await
    }

    /// Search long-term memory with the hybrid retrieval (D2-B2) and
    /// return the top-k entries. Called by the `memory_search` tool.
    /// Empty result for a query with no match; empty result when memory
    /// is disabled.
    pub async fn search_long_term(&self, query: &str, k: usize) -> Vec<kod_types::MemoryEntry> {
        let Some(manager) = &self.memory_manager else {
            return Vec::new();
        };
        match manager.retrieve_long_term_hybrid(query).await {
            Ok(v) => v.into_iter().take(k).collect(),
            Err(e) => {
                tracing::warn!(error = %e, "memory search failed");
                Vec::new()
            }
        }
    }

    /// One consolidation pass over the long-term store (design D2.5).
    /// Archives episodic entries with no touch in `ARCHIVE_AFTER_DAYS`.
    /// No-op when memory is disabled.
    ///
    /// Called by `KodEngine`'s consolidation task on the interval
    /// carried by `memory.compaction_interval_secs`. Best-effort: a
    /// store error is returned to the caller, which logs it; the task
    /// retries on its next tick.
    pub async fn consolidate_memory(&self) -> Result<kod_memory::ConsolidationReport> {
        match &self.memory_manager {
            Some(m) => m.consolidate().await,
            None => Ok(kod_memory::ConsolidationReport::default()),
        }
    }

    /// Explicitly close the memory subsystem.
    ///
    /// Consumes the router: the caller (typically `KodEngine::shutdown`)
    /// has decided the session is over. Needed because the router
    /// holds the only `MemoryManager` in the process; without a way to
    /// reach and consume it, the redb handle stays alive until the
    /// engine's `Arc<TaskRouter>` itself is dropped — which the
    /// engine cannot force from `&self`.
    ///
    /// A router built with `enable_memory: false` has no manager; the
    /// method is then a no-op after consuming the router.
    ///
    /// Idempotency is by construction: consuming `self` means the
    /// method can only be called once.
    pub fn close_memory(self) {
        let Self { memory_manager, .. } = self;
        if let Some(manager) = memory_manager {
            manager.close();
        }
    }

    /// FNV-1a hash of a canonical working directory, for scoping
    /// entries written via `memory_save` (D2-B3a). Same hash function
    /// the checkpoint manager uses for its per-project directory —
    /// one identity, one hash.
    pub fn project_key_for(working_dir: &std::path::Path) -> String {
        let canonical =
            std::fs::canonicalize(working_dir).unwrap_or_else(|_| working_dir.to_path_buf());
        crate::checkpoint::fnv1a_hex(&canonical.to_string_lossy())
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

    /// Whether a memory manager was built (i.e. `enable_memory` was
    /// true at construction). Read by the engine to decide whether to
    /// register the memory tools.
    pub fn has_memory(&self) -> bool {
        self.memory_manager.is_some()
    }

    /// Number of entries currently in short-term memory (0 when
    /// memory is disabled). Read-only; used by the engine's tests and
    /// by any future status display that wants to show working-set
    /// size.
    pub async fn get_all_short_term_len(&self) -> usize {
        match &self.memory_manager {
            Some(m) => m.get_all_short_term().len(),
            None => 0,
        }
    }

    /// Clear short-term memory. Called by `KodEngine::clear_history`
    /// so `/clear` forgets the session's turns in both stores (see
    /// that method's doc for the reasoning). Long-term memory is
    /// untouched.
    pub async fn clear_short_term_memory(&self) {
        if let Some(m) = &self.memory_manager {
            m.clear_short_term();
        }
    }

    /// Watch `skills_dir` for changes and rebuild the matcher's
    /// contents on any file-system event.
    ///
    /// The router's matcher is the single source of truth for skill
    /// lookup — `build_prompt` reads it, not any loader cache. A
    /// watcher task therefore holds a `Weak` reference to the matcher
    /// and, on each event, re-reads the whole directory and replaces
    /// the matcher contents. The `SkillWatcher` handle is stored on
    /// the router so it is not dropped the moment this returns.
    ///
    /// Debouncing is handled by reloading the whole directory on
    /// every event: a burst of events from one edit (truncate then
    /// write, common on some editors) ends up doing a handful of
    /// re-reads, and re-reading a directory of skill files is cheap.
    /// A per-file debounce would be more efficient; it would also
    /// need the watcher to know which file each event concerns and
    /// to coalesce across them, which is more machinery than the
    /// skill set size justifies.
    ///
    /// Idempotent: a second call is a no-op. No-op when the directory
    /// does not exist.
    pub async fn enable_hot_reload(&self, skills_dir: &std::path::Path) -> Result<()> {
        if !skills_dir.is_dir() {
            return Ok(());
        }
        // Already watching? One watcher per directory is enough.
        {
            let Ok(guard) = self.skill_watchers.lock() else {
                // A poisoned lock means a watcher setup panicked
                // earlier; leave hot reload off rather than risk a
                // second panic.
                return Ok(());
            };
            if !guard.is_empty() {
                return Ok(());
            }
        }

        let Some(matcher) = self.skill_matcher.clone() else {
            // No matcher means no lookup path to update.
            return Ok(());
        };

        let (watcher, mut event_rx) = kod_skills::SkillWatcher::new(skills_dir)?;
        watcher.start()?;

        let dir = skills_dir.to_path_buf();
        let weak_matcher = Arc::downgrade(&matcher);
        tokio::spawn(async move {
            while let Some(_event) = event_rx.recv().await {
                // The matcher being gone means the router was
                // dropped; exit rather than leak this task.
                let Some(matcher) = weak_matcher.upgrade() else {
                    break;
                };
                let mut loader = kod_skills::loader::SkillLoader::new(&dir);
                match loader.load_all().await {
                    Ok(skills) => {
                        let n = skills.len();
                        matcher.replace_all(skills).await;
                        tracing::info!(
                            dir = %dir.display(),
                            count = n,
                            "hot-reloaded skills"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            dir = %dir.display(),
                            error = %e,
                            "skill hot-reload failed"
                        );
                    }
                }
            }
        });

        if let Ok(mut guard) = self.skill_watchers.lock() {
            guard.push(watcher);
        }
        Ok(())
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
                let before_ok = abs == 0 || !haystack.as_bytes()[abs - 1].is_ascii_alphanumeric();
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
            "document",
            "documentation",
            "docs",
            "readme",
            "comment",
            "comments",
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

        // 2. Retrieve memory when the caller did not supply any.
        //
        // The flag computed below and the context passed to
        // `build_context` must see the *same* retrieval, or
        // `memory_used` reports on a different run than the prompt
        // did. The previous layout had the retrieval inside
        // `build_prompt` (a different method) and the flag here with
        // the caller's `None` — so the flag was always false,
        // regardless of what memory actually contributed.
        let memory_context = match memory_context {
            Some(c) => Some(c),
            None => match &self.memory_manager {
                Some(manager) => Some(manager.retrieve_context(input).await?),
                None => None,
            },
        };

        // 3. Build context is deferred to `build_prompt_with_context`,
        //    which the engine calls with the same `memory_context`
        //    computed above. Building it here (as the previous code did
        //    with `let _context = ...`) was a no-op whose only effect was
        //    a second retrieval + context build inside `build_prompt`.

        // 4. Find relevant skills
        let skills_used = self.find_relevant_skills(input).await?;

        // Did memory actually contribute to this prompt? True iff at
        // least one entry was retrieved. The previous value —
        // `memory_context.is_some()` — was true whenever the router
        // had a manager, i.e. always since `enable_memory` defaults on.
        let memory_used = memory_context
            .as_ref()
            .map(|c| !c.working_memory.is_empty() || !c.long_term.is_empty())
            .unwrap_or(false);

        // 4. The router does not generate text.
        //
        // The seven handlers this replaces returned placeholder
        // strings ("Processing simple task: …"). They existed so the
        // router could stand alone — before the engine owned
        // generation — and were the last remaining path where the
        // router answered a prompt itself. The engine's process*
        // methods now override `text` with the model's reply, so a
        // placeholder `text` here was dead weight that could leak to
        // a caller that used the router directly.
        //
        // The router now describes the task — type, skills, memory —
        // and leaves generation to whoever called it.

        // 5. Record execution time
        let execution_time_ms = start_time.elapsed().as_millis() as u64;

        Ok(TaskResponse {
            task_type,
            // The router classifies; it does not generate. `text` is
            // None here and the caller (the engine) fills it in after
            // calling the provider.
            text: None,
            tool_calls: Vec::new(),
            tool_results: Vec::new(),
            skills_used,
            memory_used,
            execution_time_ms,
            usage: None,
            memory_context,
            // The router classifies; it does not call a provider,
            // so it has no pricing to report.
            pricing: None,
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

    /// The tool definitions this router's engine exposes. Currently a
    /// thin proxy that returns an empty vec — the engine owns the
    /// actual `ToolRegistry`. A follow-up could plumb the registry in;
    /// for now the TUI's /tools-list falls back to an empty list.
    pub async fn tool_definitions(&self) -> Vec<kod_types::ToolDefinition> {
        Vec::new()
    }

    /// Build a full prompt for LLM generation. `history` is the rendered
    /// transcript of past turns (`(start of conversation)` on the first
    /// turn) — without it every prompt arrives context-free and the model
    /// opens with "this is a fresh conversation".
    /// The rendered repository map. Rebuilds (and re-renders) if the
    /// working tree has changed since the last call; otherwise returns
    /// the cached string. `None` when the working directory has no
    /// source files to map (an empty project, a non-code directory).
    ///
    /// The returned `Arc<String>` is cheap to clone and lives as long as
    /// the caller holds a reference, so it can be embedded in a
    /// `PromptPlan` without forcing the caller to re-render.
    fn repo_map_text(&self) -> Option<std::sync::Arc<String>> {
        let rendered = self
            .repo_map_cache
            .get_or_rebuild(&self.config.working_dir)?;
        if rendered.is_empty() {
            None
        } else {
            Some(rendered)
        }
    }

    /// Build a full prompt for LLM generation, retrieving memory
    /// internally. Kept for tests and callers that do not already hold a
    /// memory context. The engine uses
    /// [`TaskRouter::build_prompt_with_context`] with a context supplied
    /// by `process_input_with_context` so the retrieval runs once per
    /// prompt (D0.4).
    pub async fn build_prompt(
        &self,
        input: &str,
        task_type: &TaskType,
        history: &str,
    ) -> Result<String> {
        self.build_prompt_with_context(input, task_type, history, None)
            .await
    }

    /// Build the full prompt for LLM generation with a caller-supplied
    /// memory context. When `Some`, the retrieval is skipped — this is
    /// the single-retrieval path the engine uses. When `None`, behaves
    /// exactly like the old `build_prompt`: retrieves from the manager.
    pub async fn build_prompt_with_context(
        &self,
        input: &str,
        task_type: &TaskType,
        history: &str,
        memory_context: Option<MemoryContext>,
    ) -> Result<String> {
        self.build_prompt_with_budget(input, task_type, history, memory_context, None)
            .await
    }

    /// Full form: caller passes an explicit [`crate::budget::Allocation`]
    /// so each section is truncated to its share. The engine computes
    /// the allocation from the endpoint's window and the request size;
    /// a caller that does not care (a test, the CLI's plain path) uses
    /// the two shorter forms.
    pub async fn build_prompt_with_budget(
        &self,
        input: &str,
        task_type: &TaskType,
        history: &str,
        memory_context: Option<MemoryContext>,
        budget: Option<&crate::budget::Allocation>,
    ) -> Result<String> {
        // Truncate the request when it is over its share. The engine
        // refuses an over-budget request before reaching this point;
        // truncating here is a belt-and-braces guard so a
        // non-engine caller cannot ship an oversized prompt.
        let input = match budget {
            Some(a) if input.len() > a.request => crate::engine::truncate_chars(input, a.request),
            _ => input,
        };
        let history = match budget {
            Some(a) => crate::engine::truncate_chars(history, a.history),
            None => history,
        };
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
        prompt.push_str("## Stable prefix (cacheable)\n\n");
        if let Some(map) = self.repo_map_text() {
            let map_str = map.as_str();
            let shown = match budget {
                Some(a) => crate::engine::truncate_chars(map_str, a.repomap),
                None => map_str,
            };
            prompt.push_str("## Repository map\n\n");
            prompt.push_str(shown);
            prompt.push_str("\n\n");
        }
        prompt.push_str("## Volatile suffix (not cached)\n\n");
        // When the caller supplied a context, use it as-is; the retrieval
        // already ran in `process_input_with_context`. When `None`,
        // retrieve here (the legacy `build_prompt` path).
        let mut memory_context = match memory_context {
            Some(c) => Some(c),
            None => match &self.memory_manager {
                Some(manager) => Some(manager.retrieve_context(input).await?),
                None => None,
            },
        };
        // Truncate memory entries to the memory share.
        if let (Some(a), Some(ctx)) = (budget, memory_context.as_mut()) {
            let mut used = 0usize;
            ctx.working_memory.retain(|e| {
                if used + e.content.len() > a.memory {
                    false
                } else {
                    used += e.content.len();
                    true
                }
            });
            let mut used = 0usize;
            ctx.long_term.retain(|e| {
                if used + e.content.len() > a.memory {
                    false
                } else {
                    used += e.content.len();
                    true
                }
            });
        }
        prompt.push_str(
            &self
                .build_context(input, &memory_context, task_type)
                .await?,
        );

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
                let skill_budget = budget.map(|a| a.skills).unwrap_or(usize::MAX);
                let mut skill_used = 0usize;
                for skill_match in matches.iter().take(self.config.max_skills_per_query) {
                    let skill = &skill_match.skill;
                    // Each skill's instructions count against the
                    // skills share. When the share is exhausted, stop
                    // adding skills rather than truncate one mid-way.
                    let cost = skill.instructions.len() + skill.metadata.name.len();
                    if skill_used + cost > skill_budget {
                        break;
                    }
                    skill_used += cost;
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
                        prompt.push_str(&format!("Constraints:\n{}\n\n", constraints.trim()));
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

    /// The structured form of the same prompt `build_prompt_with_budget`
    /// returns (design §2 AD-16).
    ///
    /// Built by calling the text builder and splitting at the marker —
    /// not by duplicating the prompt-construction logic — so the two
    /// cannot drift. The `prompt_plan_renders_identically` test asserts
    /// `plan.render_text() == build_prompt_with_budget(...)` on
    /// representative inputs; a future refactor that changes one without
    /// the other fails that test.
    ///
    /// A caller that wants the plan (the eventual `CompletionRequest`
    /// migration, a provider that places cache breakpoints, a test that
    /// asserts prefix stability) uses this; a caller that still wants a
    /// `String` uses `build_prompt_with_budget` — which is exactly what
    /// this method calls under the hood, so the cost is one extra
    /// allocation.
    pub async fn build_prompt_plan(
        &self,
        input: &str,
        task_type: &TaskType,
        history: &str,
        memory_context: Option<MemoryContext>,
        budget: Option<&crate::budget::Allocation>,
    ) -> Result<PromptPlan> {
        let rendered = self
            .build_prompt_with_budget(input, task_type, history, memory_context, budget)
            .await?;
        Ok(PromptPlan::from_rendered(&rendered))
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
}

/// Repository map cache with mtime-based invalidation.
///
/// The map is not invalidated on every filesystem event — that would
/// trigger an expensive re-walk mid-conversation for an editor that
/// writes to a scratch file while a prompt is in flight. Instead, a
/// cheap fingerprint is computed at the start of every prompt and
/// compared to the last one; a mismatch triggers a rebuild of both the
/// map and its rendered form. Between calls, the cache is a plain
/// `Arc<RepoMap>` clone.
///
/// The fingerprint is FNV-1a over a sorted list of
/// `(relative_path, mtime_secs, size)` triples, walked shallow
/// (`max_depth = 3`) with .gitignore honored. Two constraints shape it:
///
/// - **Cheap.** A repo with 10 000 files would take tens of milliseconds
///   to stat every one; a shallow walk of the source tree (skipping
///   `target/`, `node_modules/`, `.git/`) sees a few hundred at most.
/// - **Stable.** Equal content on equal mtimes produces the same hash;
///   a subsequent `git checkout` restoring an identical tree is a
///   no-op rebuild.
struct RepoMapCache {
    inner: std::sync::RwLock<Option<CachedRepoMap>>,
}

struct CachedRepoMap {
    fingerprint: u64,
    /// Rendered form only. The structured `RepoMap` was stored here in
    /// the original P6 design "for D5 PageRank" but had no reader, so it
    /// was dropped (YAGNI). D5 will add it back with its first consumer;
    /// the rebuild cost is one shallow walk, already paid at
    /// invalidation time.
    rendered: std::sync::Arc<String>,
}

impl RepoMapCache {
    fn new() -> Self {
        Self {
            inner: std::sync::RwLock::new(None),
        }
    }

    /// Return the rendered map, rebuilding if the fingerprint changed.
    /// `None` when the working directory yields an empty map (no source
    /// files recognized).
    fn get_or_rebuild(&self, working_dir: &std::path::Path) -> Option<std::sync::Arc<String>> {
        let fp = fingerprint_of(working_dir);
        // Fast path: cached and unchanged.
        if let Ok(guard) = self.inner.read()
            && let Some(cached) = guard.as_ref()
            && cached.fingerprint == fp
        {
            return Some(cached.rendered.clone());
        }
        // Slow path: rebuild under the write lock. A concurrent reader
        // that wins the race sees the previous value (safe, may be
        // stale for one prompt — the next prompt re-checks).
        let map = crate::repomap::build_repo_map(working_dir);
        if map.file_count() == 0 {
            if let Ok(mut guard) = self.inner.write() {
                *guard = None;
            }
            return None;
        }
        let rendered = std::sync::Arc::new(map.render(crate::repomap::DEFAULT_MAP_CHARS));
        if let Ok(mut guard) = self.inner.write() {
            *guard = Some(CachedRepoMap {
                fingerprint: fp,
                rendered: rendered.clone(),
            });
        }
        Some(rendered)
    }
}

/// Fingerprint a working directory by (path, mtime, size) of every file
/// in a shallow walk. Uses `ignore::WalkBuilder` for the same
/// .gitignore / .ignore honoring the map itself uses — a repo with a
/// `target/` dir sees only the sources, not the build artifacts.
fn fingerprint_of(root: &std::path::Path) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h = FNV_OFFSET;

    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .hidden(false)
        .git_ignore(true)
        .ignore(true)
        .git_global(true)
        .git_exclude(true)
        .require_git(false)
        .filter_entry(|e| {
            let name = e.file_name().to_str().unwrap_or("");
            name != ".git" && name != "target" && name != "node_modules"
        })
        .max_depth(Some(3));

    // Collect (path, mtime_secs, size) into a Vec, then sort before
    // hashing so the fingerprint does not depend on walk order.
    let mut entries: Vec<(std::path::PathBuf, u64, u64)> = Vec::new();
    for entry in builder.build().filter_map(|e| e.ok()) {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let meta = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let rel = path.strip_prefix(root).unwrap_or(path).to_path_buf();
        entries.push((rel, mtime, meta.len()));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    for (path, mtime, size) in &entries {
        for byte in path.to_string_lossy().as_bytes() {
            h ^= *byte as u64;
            h = h.wrapping_mul(FNV_PRIME);
        }
        for byte in mtime.to_le_bytes() {
            h ^= byte as u64;
            h = h.wrapping_mul(FNV_PRIME);
        }
        for byte in size.to_le_bytes() {
            h ^= byte as u64;
            h = h.wrapping_mul(FNV_PRIME);
        }
    }
    h
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
    /// `build_prompt_with_context` must NOT retrieve when a context is
    /// supplied — that is the entire point of D0.4. Guard: store a
    /// sentinel fact, call the method with an empty supplied context,
    /// and assert the sentinel does not appear in the prompt. Then call
    /// with `None` and assert it does.
    #[tokio::test]
    async fn test_build_prompt_with_context_skips_retrieval_when_supplied() {
        use kod_types::MemoryContext;

        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");
        let router = TaskRouter::new(
            RouterConfig {
                embedder: None,
                skill_threshold: 0.3,
                enable_memory: true,
                max_skills_per_query: 3,
                working_dir: temp_dir.path().to_path_buf(),
                context_window: 8192,
                short_term_capacity: 100,
            },
            db_path,
        )
        .unwrap();

        let manager = router.memory_manager.as_ref().unwrap();
        manager
            .store(
                kod_types::MemoryType::LongTerm,
                "SECRET_MARKER_XYZ project fact.",
            )
            .await
            .unwrap();

        // Supplied empty context: retrieval must NOT run, marker absent.
        let prompt = router
            .build_prompt_with_context(
                "tell me about the project",
                &TaskType::Simple,
                "(start of conversation)",
                Some(MemoryContext::default()),
            )
            .await
            .unwrap();
        assert!(
            !prompt.contains("SECRET_MARKER_XYZ"),
            "build_prompt_with_context retrieved despite a supplied context"
        );

        // None: retrieval runs internally, marker present.
        let prompt = router
            .build_prompt_with_context(
                "tell me about the project",
                &TaskType::Simple,
                "(start of conversation)",
                None,
            )
            .await
            .unwrap();
        assert!(
            prompt.contains("SECRET_MARKER_XYZ"),
            "None must trigger internal retrieval"
        );
    }

    /// The repo-map cache must rebuild when the working tree changes.
    /// Regression target: the previous `OnceLock<String>` never
    /// invalidated, so a file added after the first prompt was invisible
    /// to every subsequent prompt in the same session.
    #[tokio::test]
    async fn test_repo_map_cache_rebuilds_on_change() {
        let temp_dir = TempDir::new().unwrap();
        let wd = temp_dir.path().to_path_buf();
        std::fs::write(wd.join("first.rs"), "pub fn first() {}\n").unwrap();

        let db_path = wd.join("test.redb");
        let router = TaskRouter::new(
            RouterConfig {
                embedder: None,
                skill_threshold: 0.3,
                enable_memory: false,
                max_skills_per_query: 3,
                working_dir: wd.clone(),
                context_window: 8192,
                short_term_capacity: 100,
            },
            db_path,
        )
        .unwrap();

        let first = router.repo_map_text().expect("first.rs should map");
        assert!(first.contains("first.rs"), "got: {first}");

        // Sleep one second so mtime differs (filesystems have second
        // granularity; without this the fingerprint can collide).
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        std::fs::write(wd.join("second.rs"), "pub fn second() {}\n").unwrap();

        let second = router
            .repo_map_text()
            .expect("second.rs should be mapped after rebuild");
        assert!(
            second.contains("second.rs"),
            "cache did not rebuild: {second}"
        );
        assert!(
            second.contains("first.rs"),
            "first.rs disappeared after rebuild: {second}"
        );
    }

    #[tokio::test]
    async fn test_memory_used_flag_reflects_contribution() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");
        let router = TaskRouter::new(
            RouterConfig {
                embedder: None,
                skill_threshold: 0.3,
                enable_memory: true,
                max_skills_per_query: 3,
                working_dir: temp_dir.path().to_path_buf(),
                context_window: 8192,
                short_term_capacity: 100,
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
        assert!(!resp.memory_used, "empty memory must not report as used");

        // Add a fact whose content shares a content word with the
        // next prompt, and confirm the flag flips.
        let manager = router.memory_manager.as_ref().unwrap();
        manager
            .store(
                kod_types::MemoryType::LongTerm,
                "The project is called KOD.",
            )
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
                embedder: None,
                skill_threshold: 0.3,
                enable_memory: true,
                max_skills_per_query: 3,
                working_dir: temp_dir.path().to_path_buf(),
                context_window: 8192,
                short_term_capacity: 100,
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
                embedder: None,
                skill_threshold: 0.3,
                enable_memory: true,
                max_skills_per_query: 3,
                working_dir: temp_dir.path().to_path_buf(),
                context_window: 131_072,
                short_term_capacity: 100,
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

    /// A nonexistent directory is a no-op: nothing to watch, nothing
    /// to reload. The call returns Ok and stores no watcher.
    #[tokio::test]
    async fn test_enable_hot_reload_missing_dir() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");
        let router = TaskRouter::new(
            RouterConfig {
                embedder: None,
                skill_threshold: 0.3,
                enable_memory: false,
                context_window: 8192,
                short_term_capacity: 100,
                max_skills_per_query: 3,
                working_dir: temp_dir.path().to_path_buf(),
            },
            db_path,
        )
        .unwrap();
        let missing = temp_dir.path().join("no-such-dir");
        router.enable_hot_reload(&missing).await.unwrap();
        assert_eq!(
            router.skill_watchers.lock().unwrap().len(),
            0,
            "no watcher for a nonexistent dir"
        );
    }

    /// A second call for the same directory is a no-op — one watcher
    /// per directory, not one per call.
    #[tokio::test]
    async fn test_enable_hot_reload_idempotent() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");
        let router = TaskRouter::new(
            RouterConfig {
                embedder: None,
                skill_threshold: 0.3,
                enable_memory: false,
                context_window: 8192,
                short_term_capacity: 100,
                max_skills_per_query: 3,
                working_dir: temp_dir.path().to_path_buf(),
            },
            db_path,
        )
        .unwrap();
        let dir = temp_dir.path().join("skills");
        std::fs::create_dir_all(&dir).unwrap();

        router.enable_hot_reload(&dir).await.unwrap();
        router.enable_hot_reload(&dir).await.unwrap();
        assert_eq!(
            router.skill_watchers.lock().unwrap().len(),
            1,
            "second enable must not add a second watcher"
        );
    }

    /// End-to-end: add a skill file after hot reload is enabled, wait
    /// for the filesystem event, and confirm the matcher sees the new
    /// skill. Ignored by default — `notify` event latency and coalescing
    /// vary across platforms and CI; run with `--ignored` when
    /// investigating hot reload locally.
    #[tokio::test]
    #[ignore = "filesystem event notification can be flaky in CI"]
    async fn test_hot_reload_picks_up_new_skill() {
        use std::time::{Duration, Instant};

        let temp_dir = TempDir::new().unwrap();
        let skills_dir = temp_dir.path().join("skills");
        std::fs::create_dir_all(&skills_dir).unwrap();

        // Seed one skill.
        std::fs::write(
            skills_dir.join("first.md"),
            "---\nname: first\ndescription: seed\n---\n\nbody\n",
        )
        .unwrap();

        let db_path = temp_dir.path().join("test.redb");
        let router = TaskRouter::new(
            RouterConfig {
                embedder: None,
                skill_threshold: 0.3,
                enable_memory: false,
                context_window: 8192,
                short_term_capacity: 100,
                max_skills_per_query: 3,
                working_dir: temp_dir.path().to_path_buf(),
            },
            db_path,
        )
        .unwrap();
        router.load_skills(&skills_dir).await.unwrap();
        assert_eq!(router.loaded_skill_names().await, vec!["first".to_string()]);

        router.enable_hot_reload(&skills_dir).await.unwrap();

        // Add a second skill.
        std::fs::write(
            skills_dir.join("second.md"),
            "---\nname: second\ndescription: hot-reload\n---\n\nbody\n",
        )
        .unwrap();

        // Poll for up to 2s for the matcher to include both. A fixed
        // sleep would be either too long on fast hosts or flaky on
        // loaded ones.
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut names = router.loaded_skill_names().await;
        while Instant::now() < deadline && !names.contains(&"second".to_string()) {
            tokio::time::sleep(Duration::from_millis(50)).await;
            names = router.loaded_skill_names().await;
        }
        assert!(
            names.contains(&"first".to_string()),
            "seed skill lost: {names:?}"
        );
        assert!(
            names.contains(&"second".to_string()),
            "new skill not picked up within 2s: {names:?}"
        );
    }
}
