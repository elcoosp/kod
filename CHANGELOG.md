# Changelog

All notable changes to KOD will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- **Multi-endpoint LLM routing**: named `[[llm.endpoints]]` blocks with
  per-endpoint `provider`, `base_url`, `model`, and pricing; a
  `[llm.routing]` table that selects an endpoint per task type, with a
  fallback chain. `/model` in the TUI and `--model` on the CLI resolve
  through the registry instead of rebuilding the provider.
- **Anthropic Messages API provider** (`kod-provider-anthropic`), a
  wrapper over `adk-model`'s native Anthropic client.
- **MCP client** (`kod-mcp`): spawns Model Context Protocol servers,
  lists their tools, and registers each as `mcp:<server>.<tool>` on
  the engine's tool registry. Config: `[mcp.servers.*]`.
- **LSP client** (`kod-lsp`): diagnostics, definition, references, and
  hover, plus a post-write hook that feeds the language server's
  diagnostics into the next turn's prompt automatically.
- **Landlock sandbox backend**: on Linux kernels ≥ 5.13 without
  `bwrap`, shell commands run under Landlock via the hidden
  `kod __sandbox-exec` launcher.
- **Unix-socket daemon** (`kod serve`): NDJSON protocol, peer-UID
  check, SIGINT shutdown. Attach with `--remote` on `kod prompt`,
  `kod chat`, `kod agent`, or `kod swarm`.
- **Policy engine** (`kod-config::policy`): layered presets, per-tool
  overrides, session deny rules, `.kod/policy.toml` for project-level
  policy, and `kod policy show|explain`.
- **Batch approval dialog**: one modal per round with `y` / `n` /
  `a=never` per item, `↑`/`↓` navigation, and a scrollable diff.
- **Pin & `/handoff`**: pin turns so they survive history compaction;
  generate a handoff document, write it to `.kod/handoff-<date>.md`,
  and start a fresh session with it as context.
- **Research citation verification**: `file:line` citations in
  Research replies are checked against the filesystem; a
  `## Citation check (N/M verified)` block appears when a citation is
  wrong.
- **Prompt cache budget** (`kod-core/src/budget.rs`): per-section token
  allocation with a stable cacheable prefix verified by golden tests.
- **TUI polish**: markdown rendering in assistant replies, USD cost
  display in the header, time-to-first-token in the status bar,
  `Ctrl+E` for `$EDITOR`, `/log` for the session log, `m` to toggle
  mouse capture.
- **`kod config migrate`**: brings a v1 config to `config_version = 2`,
  with a timestamped backup and an atomic write.
- **`kod policy show|explain`** subcommands.
- **Release pipeline**: musl and aarch64-linux builds, `.sha256` per
  artifact, optional minisign signatures, and a binary-size gate that
  enforces the README's 15 MB ceiling.
- **Provider contract suite** (`kod-provider::testkit`): every
  `LlmProvider` runs the same trait-level tests, plus per-provider
  request-shape tests using `httpmock`.
- **Prompt characterization snapshots** (`kod-core/tests/characterization_prompts.rs`):
  five scenario snapshots lock the router prompt's bytes so a refactor
  cannot silently change what the model sees.

### Changed
- `LlmConfig` schema is now v2: named endpoints plus a routing table.
  A v1 config still parses; run `kod config migrate` to write the v2
  shape.
- `SwarmKnowledge` and `SharedWorkspace` are gone. The swarm's
  blackboard is the `AgentCommunicationHub`; the `swarm_note` and
  `swarm_read` tools broadcast and read on it.
- `StreamChunk::ToolCallStart` / `ToolCallDelta` carry an index and an
  id, so parallel tool calls within one response assemble correctly.
- `ChatMessage` carries `tool_calls` and `tool_call_id`; the engine's
  transcript is structured, not a text buffer.

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
