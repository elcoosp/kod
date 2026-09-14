# Testing Guide

This guide covers how to test KOD, from running the automated test suite to manual verification of features.

## Quick Start

```bash
# Run the full test suite (all crates, all tests)
just test

# Or directly:
cargo test --workspace

# Run with output shown for failing tests
cargo test --workspace -- --nocapture

# Run tests in parallel (faster)
cargo test --workspace -- --test-threads=8

# Run only unit tests (inline #[cfg(test)] modules)
cargo test --workspace --lib

# Run only integration tests (tests/ directory)
cargo test --workspace --tests
```

## Test Counts

The workspace currently contains 251 tests across 12 crates (plus `#[ignore]`-gated live tests):

| Crate                  | Tests |
|------------------------|-------|
| kod-tui                | 76    |
| kod-core               | 40    |
| kod-skills             | 31    |
| kod-memory             | 24    |
| kod-tools              | 23    |
| kod-cli                | 14    |
| kod-types              | 9     |
| kod-config             | 9     |
| kod-provider-openai    | 5     |
| kod-error              | 3     |
| kod-provider           | 0     |
| **Total**              | **253** |

## Workspace Structure

The KOD workspace is organized into 12 crates:

```
crates/
  kod-types/          -- Core types (IDs, messages, tools, skills, memory)
  kod-error/          -- Error types (KodError)
  kod-config/         -- Configuration (KodConfig, LLM, Memory, Skills)
  kod-provider/       -- Provider traits (LlmProvider trait, GenerationOptions)
  kod-provider-openai/ -- OpenAI-compatible LLM provider (Ollama, LM Studio, MLX, vLLM)
  kod-skills/         -- Skill loading, parsing, matching, hot-reload watcher
  kod-memory/         -- Short-term, long-term, and episodic memory
  kod-tools/          -- Tool trait, registry, and built-in tools
  kod-tui/            -- Terminal UI (ratatui-based)
  kod-core/           -- KodEngine, TaskRouter, EngineConfig, EngineContext
  kod-cli/            -- CLI binary (kod)
```

---

## Testing Each Crate

### kod-types

Type safety and ID generation:

```bash
cargo test -p kod-types
```

Tests cover: `AgentId`, `MessageId`, `SkillId`, `MemoryId`, `ToolId`, `TaskId`, `SessionId` creation, display formatting, and string parsing. Message serialization, tool definitions, and memory types.

### kod-error

Error handling:

```bash
cargo test -p kod-error
```

Tests cover: error creation, display, and conversion via `From` traits.

### kod-config

Configuration loading/saving:

```bash
cargo test -p kod-config
```

Tests cover: default config values (`model: "codellama:13b"`, `base_url: "http://localhost:11434"`), TOML serialization round-trip, and loading from file.

**Test files:**
- `kod-config/src/config.rs` -- inline tests for `KodConfig`
- `kod-config/src/llm.rs` -- inline tests for `LlmConfig`

### kod-provider

Provider trait:

```bash
cargo test -p kod-provider
```

No tests in this crate (it only defines the `LlmProvider` trait and `GenerationOptions` struct).

### kod-provider-openai

OpenAI-compatible LLM provider (backed by `adk-model`):

```bash
# Run all provider tests (no server required; live test is #[ignore])
cargo test -p kod-provider-openai

# Run the live test against a local server (Ollama, LM Studio, MLX, ...)
cargo test -p kod-provider-openai -- --ignored

# Run with a real local server
# 1. Ollama: ollama serve (OpenAI endpoint at http://localhost:11434/v1)
# 2. LM Studio: start the server (default http://localhost:1234/v1)
# 3. Run the live test
cargo test -p kod-provider-openai -- --ignored
```

**Key APIs to know:**
- `OpenAICompatProvider::from_config(&config.llm, model_override)` -- build from kod config
- `OpenAICompatProvider::new(base_url, model)` -- server root or `.../v1` (normalized automatically)
- `OpenAICompatProvider::with_api_key(url, model, key)` -- explicit credentials
- `OpenAICompatProvider::with_model(name)` -- switch models, keep endpoint
- `LlmProvider` trait: `name()`, `list_models()`, `generate()`, `generate_with_tools()`, `stream()`

**Test files:**
- `provider.rs` -- endpoint normalization, config wiring, model switching
  (live server test is `#[ignore]`-gated)

### kod-skills

Skills system (markdown-based skill files):

```bash
# Run all skills tests
cargo test -p kod-skills

# Test specific components
cargo test -p kod-skills --test parser       # YAML front matter + markdown body parsing
cargo test -p kod-skills --test loader        # Directory scanning + caching
cargo test -p kod-skills --test matcher       # Skill matching by triggers/tags/capabilities
cargo test -p kod-skills --test watcher       # Hot-reload file watching
cargo test -p kod-skills --test examples      # Example skill files
```

**Key APIs:**
- `SkillLoader::new(skills_dir: impl Into<PathBuf>)` -- takes a directory path
- `loader.load_all(&mut self) -> Result<Vec<Skill>>` -- loads all `.md` files recursively
- `loader.get_skill(name) -> Option<Skill>`
- `loader.search(query) -> Vec<Skill>`
- `loader.count() -> usize`
- `loader.reload_skill(path) -> Result<Option<Skill>>`
- `loader.enable_hot_reload() -> Result<()>` -- spawns a file watcher
- `SkillParser::new()` / `parser.strict()` -- parse markdown skill files
- `parser.parse_content(content, source_name) -> Result<Skill>`
- `SkillMatcher::new()` -- create matcher
- `matcher.add_skill(skill)` -- add a loaded skill
- `matcher.find_relevant_skills(query) -> Vec<SkillMatch>`
- `matcher.set_max_results(n)` / `matcher.set_min_score(threshold)`

**Skill file format (YAML front matter + markdown body):**

```markdown
---
name: rust-coding
description: Rust coding assistance
version: 1.0.0
category: coding
tags:
  - rust
  - coding
capabilities:
  - code-generation
triggers:
  - "rust code"
  - "write rust"
---

## Instructions

You are a Rust coding expert. Help with idiomatic Rust code.

## Constraints

Only suggest safe, idiomatic Rust.

## Examples

<example input="How do I create a struct?">
Use struct syntax with fields...
</example>
```

**Test files:**
- `parser.rs` -- skill file parsing, validation, section extraction
- `loader.rs` -- directory scanning, caching, hot reload
- `matcher.rs` -- skill matching with trigger/tag/capability scoring
- `watcher.rs` -- file system watcher for hot reload
- `examples.rs` -- tests against `skills/examples/` markdown files
- `integration.rs` -- full pipeline tests

### kod-memory

Multi-layer memory system:

```bash
# Run all memory tests
cargo test -p kod-memory

# Test specific components
cargo test -p kod-memory --test short_term   # In-memory FIFO with capacity limits
cargo test -p kod-memory --test long_term    # Persistent redb-backed storage
cargo test -p kod-memory --test integration  # Full MemoryManager lifecycle
```

**Key APIs:**
- `MemoryManager::new(db_path: PathBuf, short_term_capacity: usize) -> Result<Self>`
- `manager.store(memory_type, content) -> Result<MemoryId>` -- `MemoryType` variants: `ShortTerm`, `LongTerm`, `Episodic`, `Semantic`
- `manager.get_short_term(&id) -> Option<MemoryEntry>`
- `manager.get_long_term(&id) -> Result<Option<MemoryEntry>>`
- `manager.search(query) -> Result<Vec<MemoryEntry>>` -- searches short-term + long-term
- `manager.retrieve_context(query) -> Result<MemoryContext>`
- `manager.clear_all() -> Result<()>`
- `ShortTermMemory::new(capacity)` -- FIFO eviction when at capacity
- `LongTermMemory::new(db_path)` -- redb-backed, persistent
- `EpisodicMemory::new()` -- in-memory with cosine similarity search

**Test files:**
- `short_term.rs` -- FIFO eviction, store/get/remove/search/clear
- `long_term.rs` -- redb CRUD operations, search persistence
- `integration.rs` -- `MemoryManager` cross-type operations

### kod-tools

Tool system:

```bash
cargo test -p kod-tools

# Test specific areas
cargo test -p kod-tools --test tools      # Built-in tool tests (read_file, write_file, etc.)
cargo test -p kod-tools --test registry   # Tool registry registration/lookup
```

**Key APIs:**
- `Tool` trait: `fn definition(&self) -> ToolDefinition` + `async fn execute(params, context) -> Result<ToolResult>`
- `ToolRegistry::new()` -- create registry
- `registry.register(Box<dyn Tool>)` -- register a tool
- `registry.get_definitions_for_llm() -> Vec<Value>` -- OpenAI-compatible format
- `registry.execute_tool(name, params, context) -> Result<ToolResult>`
- `ToolContext::new(working_dir)` -- create context with default (no) permissions
- `context.with_permissions(perms)` / `.with_timeout(secs)`
- `context.can_read(path)` / `context.can_write(path)` / `context.can_execute_command(cmd)`

**Built-in tools:**
- `ReadFileTool` -- `read_file` (params: `path`)
- `WriteFileTool` -- `write_file` (params: `path`, `content`, `append?`)
- `ListFilesTool` -- `list_files` (params: `path`, `recursive?`)
- `GrepTool` -- `grep` (params: `path`, `pattern`, `recursive?`)
- `FileInfoTool` -- `file_info` (params: `path`)
- `ExecuteCommandTool` -- `execute_command` (params: `command`)

**Test files:**
- `tools.rs` (in `src/`) -- inline tests for the Tool trait
- `tools.rs` (in `tests/`) -- integration tests for `ReadFileTool`, `WriteFileTool`, `ListFilesTool`, `FileInfoTool`, permission enforcement, missing parameters
- `registry.rs` (in `tests/`) -- registry operations: register, get, list, remove, count, LLM definitions

### kod-tui

Terminal UI:

```bash
cargo test -p kod-tui

# Test specific components
cargo test -p kod-tui --test app         # App state (messages, input, modes, agents)
cargo test -p kod-tui --test event       # Event handling (key codes, priorities, ticks)
cargo test -p kod-tui --test main_loop   # TUI lifecycle and event processing
cargo test -p kod-tui --test ui          # Widget rendering (ChatWidget, AgentPanelWidget, InputWidget)
```

**Key APIs:**
- `TuiLoop::new()` -- create TUI loop
- `tui.init_terminal()` / `tui.restore_terminal()` / `tui.run()` -- lifecycle
- `tui.handle_event(event)` -- process events
- `KodApp::new()` -- application state
- App modes: `AppMode::Normal`, `AppMode::AgentPanel`, `AppMode::ToolExecution`, `AppMode::Help`, `AppMode::Input`
- Input modes: `InputMode::Normal`, `InputMode::Insert`
- `KodApp` methods: `add_message()`, `submit_input()`, `set_input_mode()`, `set_mode()`, `start_response_stream()`, `add_response_chunk()`, `complete_response()`
- `EventHandler::new(tick_rate)` -- event handler with priority queue
- `EventHandler::push_event()`, `push_priority_event()`, `next_event()`, `send_event()`
- `Event` enum: `Key(KeyCode)`, `UserInput(String)`, `Tick`, `System(EventPriority, String)`, `ToolStarted(String)`, `ToolCompleted(String, String)`, `AgentMessage(String, String)`, `ResponseChunk(String)`, `ResponseComplete(String)`, `Error(String)`, `Quit`, `Resize(u16, u16)`
- `KeyCode` enum: `Char(char)`, `Enter`, `Escape`, `Backspace`, `Delete`, `Up`, `Down`, `Left`, `Right`, `Home`, `End`, `PageUp`, `PageDown`, `Tab`, `BackTab`, `F(u8)`

**TUI keybindings (Normal mode):**
- `i` -- enter Insert mode
- `Esc` / `q` -- quit (Esc) or enter Normal mode (from Insert)
- `Tab` -- toggle AgentPanel mode
- `?` or `h` -- show Help mode
- `a` -- switch to AgentPanel mode
- `Up`/`Down` -- scroll by 1 line
- `PageUp`/`PageDown` -- scroll by 10 lines
- `Home` -- scroll to bottom

**In Insert mode:**
- `Enter` -- submit input
- `Esc` -- return to Normal mode
- `Backspace` -- delete character before cursor
- `Up`/`Down` -- navigate input history
- Any character -- appends to input

**Test files:**
- `app.rs` -- app state management, message handling, input, modes, streaming, scroll
- `event.rs` -- event queue, priority ordering, serialization, tick events
- `main_loop.rs` -- TUI loop creation, event processing, input submission, mode switching
- `ui.rs` -- widget rendering with `ratatui` `TestBackend`

### kod-core

Core engine and task router:

```bash
cargo test -p kod-core

# Test specific components
cargo test -p kod-core --test engine      # KodEngine lifecycle, process, maintenance
cargo test -p kod-core --test router      # TaskRouter classification, routing, context
cargo test -p kod-core --test integration -- TaskType classification, engine lifecycle, memory, skills
cargo test -p kod-core --test config_integration -- EngineConfig
cargo test -p kod-core --test context    # EngineContext builder, prompt generation
```

**Key APIs:**
- `KodEngine::new(config: RouterConfig, db_path: PathBuf) -> Result<Self>`
- `engine.start()` / `engine.shutdown()` / `engine.is_running() -> Result<bool>`
- `engine.process(&input) -> Result<TaskResponse>` -- routes through TaskRouter, uses LLM provider if set
- `engine.set_provider(Arc<dyn LlmProvider>)` -- inject an LLM provider
- `engine.router() -> &TaskRouter`
- `engine.run_maintenance()` / `engine.load_skills(&path)`
- `RouterConfig` fields: `working_dir`, `enable_memory`, `max_skills_per_query`, `context_window`
- `TaskRouter::new(config, db_path)` -- create router (also creates `MemoryManager` if `enable_memory`)
- `router.classify_task(&input) -> Result<TaskType>` -- keyword-based classification
- `router.process_input(&input) -> Result<TaskResponse>`
- `router.process_input_with_context(&input, Option<MemoryContext>) -> Result<TaskResponse>`
- `router.build_prompt(&input, &TaskType) -> Result<String>`
- `router.load_skills(&skills_dir)` -- load skills into matcher

**TaskType variants:** `Simple`, `CodeModification`, `Debugging`, `Research`, `Testing`, `Documentation`, `Complex`, `MultiStep`

**Classification keywords:**
- **Complex**: "design", "architect", "implement", "create", "build", "complete", "analyze"
- **Debugging**: "debug", "error", "traceback", "panic", "exception"
- **CodeModification**: "refactor", "fix", "rename", "move", "extract", "inline", "modify", "update"
- **Testing**: "test", "verify"
- **Research**: "research", "find", "search", "look up", "investigate"
- **Documentation**: "document", "docs", "readme", "comment"
- Default: `Simple`

**TaskResponse fields:** `task_type`, `text: Option<String>`, `tool_calls`, `tool_results`, `skills_used`, `memory_used`, `execution_time_ms`

**Test files:**
- `engine.rs` -- engine creation, process, provider setup, maintenance, shutdown
- `router.rs` -- task classification for each `TaskType`, input routing, context handling, configuration
- `integration.rs` -- full pipeline: create test skills, start engine, classify tasks, verify responses
- `context.rs` -- `EngineContext` builder, prompt generation with memory/skills
- `config_integration.rs` -- `EngineConfig` from `KodConfig`, defaults, serialization

### kod-cli

CLI binary:

```bash
# Build the binary
cargo build

# Run the binary
./target/debug/kod --help

# The CLI has these subcommands:
#   kod chat       -- Start a chat session (interactive REPL)
#   kod agent      -- Run an agent with a goal
#   kod skills     -- List available skills
#   kod config     -- Show configuration
#   kod test       -- Run self-tests
#   kod tui        -- Launch the interactive terminal UI
```

**Important note:** The `kod` binary depends on the `dirs` crate (`dirs::home_dir()`) to locate the config directory at `~/.kod/`. If `dirs` is not in the dependency tree, this will fail to compile. Verify with:

```bash
cargo build -p kod-cli 2>&1 | grep "error\|warning"
```

**CLI subcommands:**

```
kod [--verbose] <SUBCOMMAND>

Subcommands:
  chat    Start a chat session
    -m, --model <MODEL>              Specify the model to use
    -t, --temperature <TEMPERATURE>  Temperature for generation (0.0-1.0) [default: 0.7]
    -i, --interactive                Start interactive REPL

  agent   Run an agent with a specific goal
    -n, --name <NAME>                Agent name [default: kod-agent]
    -g, --goal <GOAL>                Agent goal/task (required)
    -m, --model <MODEL>              Specify the model to use

  tui     Launch the interactive terminal UI
    -m, --model <MODEL>              Specify the model to use

  skills  List available skills

  config  Show configuration

  test    Run built-in self-tests
```

**CLI self-test output:**
```
Running KOD test suite...
  Config: OK (model=<your-model>)
  Engine lifecycle: OK
  Provider setup: OK (name=openai-compatible)
  Skill loading: OK (N skills) | SKIPPED (no skills directory)
```

**Test files:**
- `tests/integration_tests.rs` -- CLI help/version, config/skills commands, in-process engine/router tests
- `tests/common/mod.rs` -- `TestEnvironment` (temp dir, skills dir, config) and `run_kod_command()` helper

**In-process test APIs used in integration tests:**
- `kod_core::engine::KodEngine::new(RouterConfig, db_path)`
- `kod_core::router::TaskRouter::new(RouterConfig, db_path)`
- `kod_core::router::RouterConfig { working_dir, enable_memory, max_skills_per_query, context_window }`
- `kod_config::KodConfig::default()` / `KodConfig::load_from(path)` / `KodConfig::load_default()`
- `kod_skills::SkillLoader::new(&skills_dir)` + `loader.load_all()`
- `kod_skills::SkillMatcher::new()` + `matcher.add_skill(skill)` + `matcher.find_relevant_skills(query)`
- `kod_memory::MemoryManager::new(db_path, capacity)` + `manager.store(type, content)` + `manager.get_long_term(id)` + `manager.search(query)`

---

## Running Tests with Ollama

Several tests and the CLI require a running Ollama server:

```bash
# 1. Start Ollama (macOS with Homebrew)
ollama serve

# 2. Pull a model
ollama pull llama3.2

# 3. Run tests that need Ollama
# In-process tests use RouterConfig with enable_memory=false to avoid DB issues
cargo test -p kod-core --test integration -- --nocapture

# CLI test (requires Ollama)
cargo build
./target/debug/kod chat --model llama3.2

# The CLI config defaults to:
#   provider: Ollama
#   model: "codellama:13b"
#   base_url: "http://localhost:11434"
#   temperature: 0.7
#   timeout: 300s
```

---

## Benchmarks

Benchmarks use `criterion` and are located in `kod-core/benches/`:

```bash
# Run all benchmarks
just bench
# or
cargo bench --workspace

# Run specific benchmark groups
cargo bench -- skill_loading      # Loading 10/50/100/500 skills
cargo bench -- skill_matching     # Matching against 10/50/100 skills
cargo bench -- memory_operations  # short_term_store, short_term_retrieve
cargo bench -- agent_operations   # agent_creation
cargo bench -- task_classification # classify various input strings
cargo bench -- task_processing   # process_input for different task types
```

**Benchmark groups and functions:**

| Group                | Functions                                                                 |
|----------------------|---------------------------------------------------------------------------|
| `skill_loading`      | `load_skills` (10, 50, 100, 500 skills)                                   |
| `skill_matching`     | `match_skills` (10, 50, 100 skills)                                       |
| `memory_operations`  | `short_term_store`, `short_term_retrieve`                                 |
| `agent_operations`   | `agent_creation`                                                          |
| `task_classification`| `classify` (Simple, CodeModification, Debugging, Research, Testing, Docs, Complex) |
| `task_processing`    | `process` (simple, code_mod, debug, research)                             |

**Benchmark helpers** (`common/mod.rs`):
- `BenchEnvironment::new()` -- creates a temp directory with a `skills/` subdirectory
- `env.add_skills(count)` -- generates N skill markdown files

---

## Test Environment Setup

Tests use the following dependencies:
- `tempfile` -- temporary directories for isolation
- `tokio` -- async runtime (tests use `#[tokio::test]`)
- `rstest` -- parameterized tests (used in TUI tests)
- `serde_json` -- JSON assertions

**Common test patterns:**

```rust
// Create an engine for testing
use kod_core::router::RouterConfig;
use kod_core::engine::KodEngine;
use tempfile::TempDir;

let temp_dir = TempDir::new().unwrap();
let db_path = temp_dir.path().join("test.redb");
let config = RouterConfig {
    working_dir: temp_dir.path().to_path_buf(),
    enable_memory: true,
    max_skills_per_query: 3,
};
let engine = KodEngine::new(config, db_path).unwrap();

// Engine lifecycle
engine.start().await.unwrap();
let response = engine.process("Hello, what can you do?").await.unwrap();
engine.shutdown().await.unwrap();
```

```rust
// Create a router directly
use kod_core::router::{RouterConfig, TaskRouter, TaskType};

let router = TaskRouter::new(RouterConfig::default(), db_path).unwrap();
let task_type = router.classify_task("Fix the bug in auth.rs").await.unwrap();
assert_eq!(task_type, TaskType::CodeModification);
```

```rust
// Load and match skills
use kod_skills::SkillLoader;
use kod_skills::SkillMatcher;

let mut loader = SkillLoader::new(&skills_dir);
let skills = loader.load_all().await.unwrap();

let matcher = SkillMatcher::new();
for skill in skills {
    matcher.add_skill(skill).await;
}
let matches = matcher.find_relevant_skills("rust code").await;
```

```rust
// Test tools with permissions
use kod_tools::{ReadFileTool, ToolContext, ToolResult};
use kod_types::ToolPermissions;

let perms = ToolPermissions { read_files: true, ..Default::default() };
let context = ToolContext::new(temp_dir).with_permissions(perms);
let tool = ReadFileTool::new();
let params = serde_json::json!({ "path": "test.txt" });
let result = tool.execute(&params, &context).await.unwrap();
```

---

## Debugging Failed Tests

### Common issues:

```bash
# Run with full output
cargo test --workspace -- --nocapture

# Single test with backtrace
RUST_BACKTRACE=1 
# Run tests serially (for race conditions)
cargo test --workspace -- --test-threads=1

# Show test timing
cargo test --workspace -- --show-output

# Run only failing tests from last run
cargo test --workspace -- --last-failed
```

### Ollama not reachable:
Tests that create a `TaskRouter` with `enable_memory: false` do not require Ollama. Tests that call `engine.process()` without a provider installed get `InvalidState("No LLM provider configured…")` — install a no-op provider to exercise the pipeline.

```bash
# Skip tests that need external services
cargo test --workspace -- --skip integration

# Start Ollama before running provider/cli tests
ollama serve &
ollama pull llama3.2
```

### Permission denied in tool tests:
Default `ToolPermissions` has all flags `false`. Tests must explicitly enable permissions:

```rust
let perms = ToolPermissions { read_files: true, ..Default::default() };
let context = ToolContext::new(dir).with_permissions(perms);
```

### TUI tests in CI:
TUI tests using `ratatui::backend::TestBackend` work in headless environments. Tests that call `TuiLoop::init_terminal()` require a real TTY and should be run interactively.

---

## Pre-Release Checklist

```bash
#!/bin/bash
# pre_release_checklist.sh

echo "KOD Pre-Release Testing Checklist"
echo "======================================"

# 1. Code quality
cargo fmt --all -- --check && echo "  Formatting OK"
cargo clippy --workspace --all-targets -- -D warnings && echo "  Clippy OK"
cargo build --workspace && echo "  Build OK"

# 2. Unit tests
cargo test --workspace --lib && echo "  Unit tests pass"

# 3. Integration tests (no external services required)
cargo test --workspace --tests && echo "  Integration tests pass"

# 4. Skill validation (if skills dir exists)
echo "  Skills: manual check via 'kod skills'"

# 5. Release build
cargo build --release && echo "  Release binary OK"
./target/release/kod --version && echo "  Binary runs"

echo ""
echo "All checks completed!"
```

---

## Summary: Recommended Testing Order

```bash
# 1. Fast: all unit + integration tests (no external services needed)
cargo test --workspace

# 2. With Ollama (if available)
ollama serve &
ollama pull llama3.2
cargo test -p kod-provider-openai -- --ignored
cargo test -p kod-cli --test integration_tests

# 3. Benchmarks
cargo bench --workspace

# 4. Lint + format
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

**Total time: ~5-10 minutes for full test suite (no Ollama), ~15-20 minutes with Ollama.**
