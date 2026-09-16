# Changelog

All notable changes to KOD will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- `kod doctor` — first-run diagnostics (config, endpoint, skills, memory, git)
- `kod init` — onboarding wizard output
- `kod models` — list provider models with `--filter`
- `kod sessions` — inspect/export/clear the TUI session file
- `kod completions <shell>` — bash/zsh/fish/elvish/powershell completion
- `kod checkpoint list|restore|clear` — file-level undo for `write_file` /
  `patch_file`; snapshots taken before every mutating call
- `kod update` — check GitHub for a newer release (read-only)
- `kod skills --json` and `kod skills-validate` — machine-readable skill
  inventory and CI-friendly validation
- `web_fetch` tool — HTTP/HTTPS fetch with SSRF protection
  (loopback / private / link-local / metadata endpoints refused)
- `git_status` and `git_diff` tools — read-only git inspection
- `tools.confirm_writes` — opt-in write approval; TUI dialog and CLI stdin
  prompt
- `llm.network_access` — opt-in network access for `web_fetch`
- Approval dialog in TUI (`y`/`n`/Esc); CLI chat prompts on stdin
- `--sandbox` flag on `kod chat` — run shell commands under bwrap/sandbox-exec
- Session log (`KOD_SESSION_LOG`) and `kod replay` — every tool call recorded
  as JSONL, replayable without the model
- Unified-diff attachment on `write_file` / `patch_file` results

### Changed
- `Engine::process()` without a provider now returns an `InvalidState` error
  instead of the router's placeholder text
- `LlmConfig::validate()` clamps out-of-range values (temperature, context
  window, max tokens, timeout, base URL, model)

### Fixed
- Engine agentic loop no longer holds the provider read lock across the
  streaming round — `/model` switch during a prompt is now instantaneous
- Provider `list_models()` errors are no longer collapsed to an empty list
- Config `load_default()` is forgiving of a corrupt file (warns and falls
  back to defaults instead of exiting)
- Multiple substring-matcher and word-boundary fixes in `TaskRouter::classify_task`

### Removed
- Placeholder handlers in `TaskRouter::process_input` that returned
  fabricated text instead of failing loudly

## [0.1.0] - 2026-09-04

### Added
- Project structure and workspace setup
- Core type definitions (ids, messages, skills, memory, tools)
- Error handling system
- Configuration management
- LLM provider abstraction
- Ollama provider implementation
- Skills system:
  - Markdown parsing with YAML front matter
  - Skill loader with directory scanning
  - Skill matcher with pattern-based matching
  - Hot reloading with file system watching
- Memory system:
  - Short-term memory (in-memory with capacity limits)
  - Long-term memory (redb persistent storage)
  - Episodic memory (vector-based for semantic search)
  - Memory manager with unified interface
  - Context builder for LLM prompts
- Tool system:
  - Tool registry and trait definitions
  - File system tools (read, write, list)
  - Git tools (status, diff)
  - Tool executor with timeout handling
  - Permission-based sandboxing
- Agent swarm:
  - Agent lifecycle management
  - Direct messaging between agents
  - Shared workspace with file locking
  - Task coordination and decomposition
  - Agent swarm manager
- TUI:
  - Event handling system
  - Application state management
  - Chat widget with message display
  - Agent panel
  - Input handling with history
  - Streaming response display
- CLI:
  - Command structure (chat, skills, config)
  - Command handlers
  - Main entry point
  - Configuration integration
  - Verbose mode with build info
- Integration tests
- Performance benchmarks
- CI/CD pipelines
- Documentation (README, CHANGELOG, CONTRIBUTING, ARCHITECTURE)
