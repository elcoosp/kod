# Changelog

All notable changes to KOD will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]
### Added
### Fixed
- **`deny.toml` now carries a real policy** (design §5.3 B4):
  the file existed but its `[licenses]` section carried no
  allow-list and its `[sources]` section was missing entirely, so
  `cargo deny check` passed trivially and enforced nothing. The
  new policy adds an explicit licence allow-list covering the
  licenses in the dependency tree (MIT, Apache-2.0 with and
  without the LLVM-exception, BSD, ISC, MPL-2.0, CC0-1.0,
  Unlicense, Unicode, BSL-1.0, Zlib); a `[sources]` section that
  denies any registry other than crates.io and any git
  dependency; an `[advisories]` section that blocks on
  vulnerabilities and warns on `unmaintained`/`unsound`; and a
  small `[bans]` deny list for `openssl-sys` (the workspace uses
  rustls) and `fastembed` (removed as a phantom dependency). A
  licence outside the allow-list is now a build failure at PR
  time, which is the whole point of the check.

- **Live Anthropic API tests + skippable CI job** (design §4
  D1.5): `crates/kod-provider-anthropic/tests/live.rs` carries
  four `#[ignore]`d tests — a text completion, a streaming text
  completion, a tool-use round trip (which exercises the
  `tool_call_id` plumbing against the real wire), and a
  two-call cache-control sanity check. Every test re-checks
  `ANTHROPIC_API_KEY` and returns cleanly when it is absent, so a
  maintainer who runs `--ignored` locally without the variable
  set sees a skip rather than a failure. The CI job
  `live-anthropic` runs the same command gated on the repository
  secret — a fork PR is skipped, a maintainer push runs — which
  is the design's explicit "skip, do not fail" contract.
- **`kod doctor` reports the `[lsp]` section** (design §4 D5.2):
  two new checks — `lsp.auto_diagnostics` reports the effective
  post-write diagnostics switch and names the config knob that
  disabled it when it is off; `lsp.settle_ms` warns on values
  outside the recommended 200 ms–10 s range. Closes the loop on
  the config section: it is now visible in `kod doctor`, not only
  in `kod config show-merged`.
- **`kod memory add <text>`
- **`/check` and `/handoff` slash-command coverage**: two
- **Swarm panel and `@N` focus state coverage**: `KodApp`'s swarm
- **`kod serve` NDJSON round-trip test**
- **The tool-only reply summary now sees the tool results**: the
- **TUI slash-command coverage: `/policy`, `/map`, `/grep`,
- **A project's `.kod/policy.toml` can now explicitly pin
- **`stream_completion` default and `tokens_per_sec` tests**:
- Engine agentic loop no longer holds the provider read lock across the
- Provider `list_models()` errors are no longer collapsed to an empty list
- Config `load_default()` is forgiving of a corrupt file (warns and falls
- Multiple substring-matcher and word-boundary fixes in `TaskRouter::classify_task`
- **Sandbox status mapping + containment tests** (design §11.3,
- **Structured-prompt and capability-routing integration tests**:
- **`kod config migrate` + `kod policy explain` end-to-end tests**:
- **`/map` + `/grep` TUI handlers, cold-start bench, PR size gate**:
- **`[lsp]` config section** (design §4 D5.2): two new knobs.
- **`LspManager` unit tests**
- **`SwarmRunner::from_config` and `kod acp` test coverage**: the
- **Consolidation tests + deterministic embedder mock**
- **`kod acp` + TUI `/policy`** (design §11.2, §6.2): a new
- **`[tools] preset` is now honoured** (design §6.1): the global
- **Consolidation now fuses near-duplicate entries** (design D2.5,
- **Engine calls the structured provider methods** (design §2
- **AD-15 complete: `Approval`, `MemoryWrite`, `Diagnostics` are
- **CI: dedicated contracts + golden jobs** (design §11.4): the
- **Anthropic wire-level cache stability test**
- **Sandbox badge + streaming rate in the TUI** (design D3.3,
- **`stream_completion` added to the trait contract suite**
- **Engine migrated to `CompletionRequest`** (design §2 AD-01, §4
- **Swarm run-budget knobs are now read from the config**
- **Working default for `LlmProvider::stream_completion`**: the
- **Provider request-shape contract tests** (design D1.5, §11.2):
- **Anthropic `stream_completion`** (design §4 D1.2, A5b): the
- **`memory.embedding_endpoint` is now read** (design D2.1): the
- **`LlmProvider::stream_completion`** (design §2 AD-01, §4 D1.4
- **Markdown render cache** (design D6.5): the chat widget
- **Swarm heartbeat watchdog** (design D4.3): the agent's streaming
- **Capability-pool swarm dispatch** (design D4.3): the swarm
- **`kod policy forget <n>`** (design §6.2): the CLI verb the
- **Anthropic `cache_control` on the last cacheable system segment**
- **`PromptPlan` (design §2 AD-16)**: `TaskRouter::build_prompt_plan`
- **Swarm runner honours the planner's capability assignment**
- **Policy-default + metrics test coverage**: new integration tests
- **Global run timeout for `kod swarm`** (design D4.3):
- **Periodic memory consolidation** (design D2.5):
- **Agent panel shows each swarm agent's model, and `@N` focus**
- **Per-capability swarm model routing** (design D1.4 PR A7):
- **Explicit redb close on shutdown** (design D0.3).
- **Multi-language LSP pool wired into the engine**
- **Multi-language LSP pool** (`kod_lsp::LspManager`): one
- **Golden prefix test** (`crates/kod-core/tests/golden_prefix.rs`):
- **`can_execute_command` no longer pattern-matches dangerous
- **`/debug tokens` reports the prompt allocation table** (AD-14):
- **`auto_lsp` is now independent of `auto_check`**: a successful
- **`/remember <text>`** in the TUI: stores a durable fact with the
- **Multi-endpoint LLM routing**: named `[[llm.endpoints]]` blocks with
- **Anthropic Messages API provider** (`kod-provider-anthropic`), a
- **MCP client** (`kod-mcp`): spawns Model Context Protocol servers,
- **LSP client** (`kod-lsp`): diagnostics, definition, references, and
- **Landlock sandbox backend**: on Linux kernels ≥ 5.13 without
- **Unix-socket daemon** (`kod serve`): NDJSON protocol, peer-UID
- **Policy engine** (`kod-config::policy`): layered presets, per-tool
- **Batch approval dialog**: one modal per round with `y` / `n` /
- **Pin & `/handoff`**: pin turns so they survive history compaction;
- **Research citation verification**: `file:line` citations in
- **Prompt cache budget** (`kod-core/src/budget.rs`): per-section token
- **TUI polish**: markdown rendering in assistant replies, USD cost
- **`kod config migrate`**: brings a v1 config to `config_version = 2`,
- **`kod policy show|explain`** subcommands.
- **Release pipeline**: musl and aarch64-linux builds, `.sha256` per
- **Provider contract suite** (`kod-provider::testkit`): every
- **Prompt characterization snapshots** (`kod-core/tests/characterization_prompts.rs`):
- Placeholder handlers in `TaskRouter::process_input` that returned
- **`kod doctor` reports detected language servers** (one row per
- `LlmConfig` schema is now v2: named endpoints plus a routing table.
- `SwarmKnowledge` and `SharedWorkspace` are gone. The swarm's
- `StreamChunk::ToolCallStart` / `ToolCallDelta` carry an index and an
- `ChatMessage` carries `tool_calls` and `tool_call_id`; the engine's
- `kod doctor` — first-run diagnostics (config, endpoint, skills, memory, git)
- `kod init` — onboarding wizard output
- `kod models` — list provider models with `--filter`
- `kod completions <shell>` — bash/zsh/fish/elvish/powershell completion
- `kod checkpoint list|restore|clear` — file-level undo for `write_file` /
- `kod update` — check GitHub for a newer release (read-only)
- `kod skills --json` and `kod skills-validate` — machine-readable skill
- `web_fetch` tool — HTTP/HTTPS fetch with SSRF protection
- `git_status` and `git_diff` tools — read-only git inspection
- `tools.confirm_writes` — opt-in write approval; TUI dialog and CLI stdin
- `llm.network_access` — opt-in network access for `web_fetch`
- Approval dialog in TUI (`y`/`n`/Esc); CLI chat prompts on stdin
- `--sandbox` flag on `kod chat` — run shell commands under bwrap/sandbox-exec
- Unified-diff attachment on `write_file` / `patch_file` results
- `Engine::process()` without a provider now returns an `InvalidState` error
- `LlmConfig::validate()` clamps out-of-range values (temperature, context

### Fixed
- **Episodic facts are now attributed to a session** (design D2.5):
- **Collected loop also drains steers at the top of the round**:
- **Routing table keys are now validated** (design §2 AD-06, §12):
- **Tool rounds and goal turns accumulate on the structured path**:
- **`/steer` now reaches the model on the structured path**: the
- **`KodConfig::default()` now stamps `config_version = 2`** —
- **Session log + `kod replay` compatibility tests** (design §11.3,
- **Streaming, session-log, and HTML-export tests**: three
- **End-of-session extraction writes Episodic entries** (design
- **Cacheable/volatile split preserved through to the wire**
- **No double-send of the user turn on the structured path**: the
- **`@N` focus + session deny-rule tests**: unit tests for
- **Structured tool rounds in the transcript** (design §2 AD-02,
- **Default policy preset is now `standard`** (writes require
- **Sandbox defaults to `Auto`**: `bwrap` / `sandbox-exec` /
- `kod sessions` — inspect/export/clear the TUI session file
- Session log (`KOD_SESSION_LOG`) and `kod replay` — every tool call recorded

### Removed
- **Placebo config fields and dead CLI flags** (design §5.3
- **`/export-html` + phantom-dep removal** (design H1, D6.5): the

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
