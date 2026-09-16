<div align="center">
  <img src="docs/logo.png" alt="KOD Logo" width="200"/>
  <p>
    <strong>A local-first AI coding agent harness for the terminal, written in Rust.</strong><br/>
    Multi-crate Cargo workspace with a full agent loop: skills, multi-layer memory, tool calling, streaming LLM providers, and both a TUI and a scriptable CLI. Point it at Ollama, LM Studio, MLX, vLLM, or any OpenAI-compatible endpoint — the model, the memory, and the tool loop all stay on your machine.
  </p>
  <p>
    <img src="https://img.shields.io/badge/Rust-1.75%2B%20%7C%202024-000000?style=flat-square&logo=rust" alt="Rust"/>
    <img src="https://img.shields.io/badge/License-MIT-blue?style=flat-square" alt="License MIT"/>
    <img src="https://img.shields.io/badge/Crates-12-6F4E37?style=flat-square" alt="Crates"/>
    <img src="https://img.shields.io/badge/Backend-Ollama%20%7C%20OpenAI--Compatible-6A0DAD?style=flat-square" alt="Backend"/>
    <img src="https://img.shields.io/badge/Interface-TUI%20%2B%20CLI-4B32C3?style=flat-square" alt="Interface"/>
    <img src="https://img.shields.io/badge/Skills-Markdown%20%2B%20Hot%20Reload-00BFFF?style=flat-square" alt="Skills"/>
    <img src="https://img.shields.io/badge/Memory-Short%20%2B%20Long%20Term-FF4500?style=flat-square" alt="Memory"/>
    <img src="https://img.shields.io/badge/Tools-FS%20%7C%20Git%20%7C%20Shell-333333?style=flat-square" alt="Tools"/>
    <img src="https://img.shields.io/badge/Agentic%20Loop-Streaming%20%2B%20Tools-007ACC?style=flat-square" alt="Agentic Loop"/>
    <img src="https://img.shields.io/badge/Swarm-Multi--Agent-228B22?style=flat-square" alt="Swarm"/>
    <img src="https://img.shields.io/badge/Sandbox-bwrap%20%7C%20sandbox--exec-8B0000?style=flat-square" alt="Sandbox"/>
    <img src="https://img.shields.io/badge/Build-Passing-brightgreen?style=flat-square&logo=githubactions" alt="Build"/>
  </p>
</div>

---

# KOD

> [!NOTE]
> KOD is early-stage. The workspace builds, tests pass, and the CLI and TUI are usable, but the public API is still stabilising. See [Project Status](#project-status) for what is wired up today versus what is still on the roadmap.

---

## Table of Contents

- [Why local-first](#why-local-first)
- [Features](#features)
- [Architecture](#architecture)
- [Getting Started](#getting-started)
- [Usage](#usage)
- [Configuration](#configuration)
- [Skills](#skills)
- [Development](#development)
- [Testing](#testing)
- [Project Status](#project-status)
- [License](#license)

---

## Why local-first

Your code does not leave your machine.

This is the difference between KOD and most terminal coding agents. Cloud-hosted agents are built around a model you do not control, on a schedule you do not set, under terms that can change without your consent. When a vendor deprecates a model, changes a rate limit, or has a bad day, your workflow stops.

KOD is built around the opposite assumption:

- **The binary is one Rust program.** `cargo build --release` produces a single executable that runs the agent, the tools, the memory store, and the terminal UI. No Node runtime, no Python interpreter, no Docker.
- **The model is one you chose.** The default config points at `http://localhost:11434/v1` (Ollama). Any OpenAI-compatible endpoint works — LM Studio, MLX Omni Serve, vLLM, OpenAI itself — but only if you want cloud.
- **Memory is on disk, in a file you can inspect.** Long-term memory lives in a `redb` database under `~/.kod/`, or per-project under `<cwd>/.kod/` when `memory.scope = "project"`.
- **The tool loop is auditable.** `kod replay <session.jsonl>` re-runs the tool calls a session made, without the model. Every `read_file`, `write_file`, and `execute_command` is checkable against the codebase at any later commit.

If "where does my code go when I run this?" is a question you want a one-line answer to, KOD is for you.

---

## Features

### Agent harness

- **Skills system** — Markdown files with YAML front matter, matched by triggers, tags, capabilities, and name. Hot-reloads on edit.
- **Multi-layer memory** — Short-term (in-memory, FIFO) and long-term (`redb`, persistent). Global or project-scoped, with word-overlap retrieval against the user's prompt.
- **Tool calling** — File system (`read_file`, `write_file`, `patch_file`, `list_files`, `grep`, `file_info`), git (`git_status`, `git_diff`), and shell (`execute_command`). Every tool is permission-gated through `ToolContext`.
- **Streaming agentic loop** — The engine classifies tasks, builds a grounded prompt (identity + environment + repo map + skills + history), streams tokens live, executes tool calls, and feeds results back up to a configurable round cap.
- **Agent swarm** — `kod swarm` decomposes a goal into N subtasks, runs them concurrently, detects file conflicts, and asks the model to merge the results.
- **Optional sandboxing** — `--sandbox` runs shell commands through `bwrap` (Linux) or `sandbox-exec` (macOS), failing loudly if the primitive is unavailable rather than silently running unsandboxed.
- **Session logs and replay** — Every tool call is recorded as JSONL and can be re-executed with `kod replay` as a regression check.
- **Model profiles** — `kod profile` ships presets for common local and cloud models.

### Interfaces

- **TUI** — A full ratatui interface with chat, live tool rows, agent panel, search, themes, slash commands, streaming responses, and a context meter.
- **CLI** — A scriptable command-line interface with `chat`, `agent`, `swarm`, `skills`, `config`, `models`, `profile`, `sessions`, `map`, `replay`, `doctor`, `init`, `test`, and shell completions.

### LLM providers

- **OpenAI-compatible, single code path** — Ollama's `/v1`, LM Studio, MLX Omni Serve, vLLM, and OpenAI all speak the same chat-completions wire protocol. Base URLs are normalised automatically, so a server root or an `.../v1` root both work.
- **Streaming with tool calls** — SSE token streaming with `ToolCallStart` / `ToolCallDelta` framing, so tool calls are assembled live, not buffered.

---

## Architecture

KOD is a Cargo workspace. Each crate has a single responsibility and a narrow public API, so each phase can be tested in isolation.

| Crate | Description |
|-------|-------------|
| `kod-types` | Shared types: strongly-typed IDs, messages, skills, memory, tools. |
| `kod-error` | `KodError` enum and `Result` alias. |
| `kod-config` | `KodConfig`, `LlmConfig`, `MemoryConfig`, `SkillsConfig`, `SwarmConfig`, model profiles. |
| `kod-provider` | `LlmProvider` trait, `GenerationOptions`, streaming chunk types. |
| `kod-provider-openai` | OpenAI-compatible implementation (backed by `adk-model`). |
| `kod-skills` | Skill parser, loader, matcher, and hot-reload watcher. |
| `kod-memory` | Short-term memory and long-term `redb` storage. |
| `kod-tools` | `Tool` trait, registry, built-in tools, per-path lock table, sandbox invocation. |
| `kod-swarm` | Agent lifecycle, communication hub, task coordinator, shared workspace. |
| `kod-core` | `KodEngine`, `TaskRouter`, `SwarmRunner`, repository map, session log, hooks. |
| `kod-tui` | Terminal UI: app state, event handling, widgets, keybindings, themes. |
| `kod-cli` | The `kod` binary: command definitions and handlers. |

### Data flow

```
user input
   │
   ▼
┌────────────────┐   ┌────────────────┐   ┌────────────────┐
│  TaskRouter    │──▶│  Memory +      │──▶│  Grounded      │  (kod-core, kod-memory, kod-skills)
│  (classify)    │   │  Skill match   │   │  prompt        │
└────────────────┘   └────────────────┘   └────────────────┘
                                                │
                                                ▼
┌────────────────┐   ┌────────────────┐   ┌────────────────┐
│  Tool results  │◀──│  LLM provider  │──▶│  Streaming     │  (kod-provider, kod-tools)
│  fed back      │   │  (streaming)   │   │  chunks → UI   │
└────────────────┘   └────────────────┘   └────────────────┘
        │
        ▼
┌────────────────┐   ┌────────────────┐
│  Final reply   │──▶│  Session log   │  (kod-core::session_log, replay)
│  → CLI / TUI   │   │  (JSONL)       │
└────────────────┘   └────────────────┘
```

See [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for the full crate-by-crate breakdown, and [`docs/SPEC.md`](docs/SPEC.md) for the longer design document.

---

## Getting Started

### Prerequisites

- **Rust** 1.75 or newer (install via [rustup](https://rustup.rs/)). The workspace uses the 2024 edition.
- **A model server** — [Ollama](https://ollama.ai/) is the recommended default, but any OpenAI-compatible endpoint works.
- **Git** — required for the git tools and for `build.rs` (version stamping).

### From source

```bash
git clone https://github.com/kod-team/kod.git
cd kod
cargo build --release
```

The binary lands at `./target/release/kod`.

### First run

```bash
# 1. Make sure your model server is running
ollama serve

# 2. Pull a model
ollama pull qwen2.5-coder:7b

# 3. Verify the setup
./target/release/kod doctor

# 4. Point KOD at a model
./target/release/kod profile use local-fast

# 5. Start a session
./target/release/kod tui
```

`kod init` will walk you through the same steps and write a default config to `~/.config/kod/config.toml` if one does not exist.

---

## Usage

### CLI

```
kod [--verbose] <COMMAND>

Commands:
  chat         Start a chat session (plain REPL)
  tui          Launch the interactive terminal UI
  agent        Run a single agent against a goal
  swarm        Decompose a goal, run N agents, merge results
  skills       List loaded skills
  config       Show the effective configuration
  models       List models the configured provider offers
  profile      List, show, or switch model profiles
  sessions     Inspect, export, or clear the saved TUI session
  map          Print the repository map
  replay       Re-run the tool calls from a session log
  doctor       Print a diagnostics report
  init         First-run helper
  test         Run the built-in self-tests
  completions  Print a shell completion script
```

A few worth calling out:

```bash
# Chat with streaming output
kod chat --model qwen2.5-coder:7b

# One-shot agent run
kod agent -g "add a doc comment to every public function in src/lib.rs"

# Run a swarm on a larger goal
kod swarm -g "implement and test a rate limiter for the HTTP client" -n 4

# Switch to a cloud model
kod profile use cloud-openai

# Replay a recorded session
kod replay ~/.kod/sessions/1730000000000.jsonl --execute
```

### TUI

Launch with `kod tui` (or just `kod` for the entry-point summary). The interface has a header, chat area, status line, and input box.

**Key bindings** (normal mode):

| Key | Action |
| --- | --- |
| `i` | enter insert mode |
| `Esc` | cancel the running prompt (in normal mode) |
| `j` / `k` | scroll down / up |
| `g` / `G` | jump to oldest / newest |
| `PgUp` / `PgDn` | scroll by page |
| `t` | toggle tool-output visibility |
| `o` | expand the newest collapsed tool row |
| `f` | start a type-ahead search |
| `n` / `N` | next / previous search match |
| `y` | copy the last assistant reply |
| `r` | retry the last prompt |
| `e` | edit your last message |
| `u` | undo a `/clear` |
| `?` | help overlay |
| `q` | quit (asks for confirmation when a generation is running) |

**In insert mode:** `Enter` sends, `Ctrl+J` (or `Shift+Enter`) inserts a newline, `Tab` cycles completions, `Up`/`Down` walk prompt history, `Ctrl+W` / `Ctrl+U` / `Ctrl+K` edit the line, `Ctrl+Left` / `Ctrl+Right` jump by word.

**Slash commands:**

```
/help      /clear     /model [<name>]   /skills    /goal <text>
/steer     /cancel    /compact          /retry     /search [<text>]
/copy      /theme     /tools            /undo      /edit
/debug     /swarm     /quit
```

> [!TIP]
> Typing a plain prompt while a generation is running steers it — the same as `/steer`. This is the fastest way to redirect a run that is heading the wrong direction without losing the work it has already done.

### Shell completions

```bash
# Bash
kod completions bash > ~/.local/share/bash-completion/completions/kod

# Zsh
kod completions zsh > ~/.zfunc/_kod

# Fish
kod completions fish > ~/.config/fish/completions/kod.fish
```

---

## Configuration

KOD reads its config from `~/.config/kod/config.toml` (Linux) or `~/Library/Application Support/kod/config.toml` (macOS). The file is auto-generated on first run.

```toml
[llm]
provider        = "OpenAICompatible"   # or "Ollama" / "OpenAI" (aliases)
model           = "qwen2.5-coder:7b"
base_url        = "http://localhost:11434/v1"
context_window  = 32768
max_tokens      = 4096
temperature     = 0.2
timeout_secs    = 120

[memory]
short_term_capacity = 100
scope               = "global"         # or "project"
# long_term_db_path = "/custom/path.redb"  # overrides scope

[skills]
enable_hot_reload     = true
max_skills_per_query  = 3
# skills_dir = "/custom/skills"        # overrides the standard search paths

[swarm]
max_agents     = 5                     # clamped to 2–8 by the runner
merge_results  = true

[hooks]
enabled = false
# pre_tool_use  = { write_file = "rustfmt {path}" }
# post_tool_use = { write_file = "cargo check" }
```

Out-of-range values are clamped with a warning at load time, so a typo does not produce a mysterious failure three prompts later.

> [!NOTE]
> The `provider` field accepts `OpenAICompatible`, `Ollama`, and `OpenAI` — the last two are aliases for the first. Any server that speaks the OpenAI chat-completions protocol works on the same code path: Ollama's `/v1`, LM Studio, MLX Omni Serve, vLLM, and OpenAI itself.

### Model profiles

The `kod profile` subcommand ships a small set of presets so a first-run user does not have to know what a `context_window` is to get a working session.

```bash
kod profile list           # show the built-in presets
kod profile show           # print the effective [llm] config
kod profile use local-fast # write the preset's values into config.toml
```

Presets: `local-fast` (Qwen 2.5 Coder 7B), `local-capable` (32B), `local-reasoning` (DeepSeek-R1 14B), `cloud-openai` (GPT-4o mini).

---

## Skills

Skills are Markdown files with YAML front matter. KOD discovers them in these directories, in order; later directories shadow earlier ones for skills that share a name:

1. `~/.kod/skills` — canonical KOD location
2. `~/.agents/skills` — Claude-style compatibility
3. `<project>/.kod/skills` — project-local KOD
4. `<project>/.agents/skills` — project-local Claude-style

Set `skills.skills_dir` in the config to use a single custom directory instead.

### Skill file format

```markdown
---
name: rust-refactoring
description: Rust refactoring with idiomatic patterns
version: 1.0.0
category: coding
tags: [rust, refactoring, idioms]
capabilities: [code-refactoring, ownership-analysis]
triggers:
  - "refactor rust"
  - "make more idiomatic"
---

## Instructions

You are an expert Rust refactoring assistant...

## Examples

<example input="Refactor this loop">
...
</example>

## Constraints

- Never change public API without explicit request
- Preserve existing tests
```

The parser splits the body into instructions, examples, and constraints, and all three reach the model when the skill is matched. Hot reload is on by default — edit a skill file and the next prompt sees it.

`kod skills` and `/skills` inside the TUI both list the merged set from every standard directory.

---

## Development

```bash
# Build
cargo build --workspace

# Run all tests
cargo test --workspace

# Lint and format
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings

# Run the full check suite (format + lint + test + release build)
./scripts/test.sh

# Benchmarks
cargo bench --workspace
```

A `justfile` provides shortcuts: `just build`, `just test`, `just lint`, `just check`, `just bench`.

### Live model tests

Some tests exercise the real provider against a running server. They are `#[ignore]`-gated so `cargo test` stays fast and offline:

```bash
# Start Ollama, pull a small model, then:
KOD_TEST_MODEL=qwen2.5:0.5b \
  cargo test -p kod-provider-openai -- --ignored --nocapture

KOD_TEST_MODEL=qwen2.5:0.5b KOD_TEST_DB=/tmp/kod-test.redb \
  cargo test -p kod-tui --test main_loop -- --ignored --nocapture
```

---

## Testing

The workspace contains roughly 356 test attributes across 11 crates. `cargo test --workspace` runs unit tests, integration tests, and doc tests without external services.

| Crate | Description |
|-------|-------------|
| `kod-types` | IDs, message serialization, tool and skill types. |
| `kod-error` | Error creation, display, conversion. |
| `kod-config` | Defaults, partial-config parsing, LLM validation, profiles, memory scope. |
| `kod-provider` | (Trait-only; no tests.) |
| `kod-provider-openai` | Endpoint normalisation, config wiring, model switching. Live test is `#[ignore]`-gated. |
| `kod-skills` | Parser, loader, matcher, watcher, and full-pipeline tests. |
| `kod-memory` | Short-term FIFO eviction, redb round-trips, retrieval, compaction. |
| `kod-tools` | Built-in tool tests, path resolution, sandbox invocation, permission enforcement. |
| `kod-core` | Engine lifecycle, task classification, memory wiring, prompt grounding, tool rounds, session log, swarm runner. |
| `kod-tui` | App state, event handling, keybindings, completion, search, streaming, main loop. |
| `kod-cli` | Doctor checks, command dispatch, in-process engine tests. |

To see the exact current total:

```bash
cargo test --workspace -- --list | tail -1
```

See [`docs/TESTING.md`](docs/TESTING.md) for the full testing guide, including per-crate test inventories and CI behaviour.

---

## Project Status

### Working end-to-end

- Workspace builds on Linux, macOS, and Windows.
- CLI commands: `chat`, `tui`, `agent`, `swarm`, `skills`, `config`, `models`, `profile`, `sessions`, `map`, `replay`, `doctor`, `init`, `test`, `completions`.
- OpenAI-compatible provider with live SSE streaming and tool-call assembly.
- Skills loading from four standard directories with hot reload.
- Short-term and long-term memory with project-scoped or global storage.
- File system, git, and shell tools with permission gating and per-path advisory locks.
- Agentic streaming loop with tool rounds, cancellation, steering, and goal looping.
- Multi-agent swarm runner with decomposition, conflict detection, and merge.
- Session log (JSONL) and `kod replay` for tool-call re-execution.
- Optional shell sandboxing through `bwrap` / `sandbox-exec`.
- Shell hooks (`pre_tool_use`, `post_tool_use`) driven by config templates.
- TUI with chat, live tool rows, agent panel, search, themes, keybindings, and slash-command completion.

### Tracked gaps

- Only the OpenAI-compatible protocol is implemented. `Anthropic` and `Custom` provider variants are recognised in config but will fail at the first prompt; `LlmConfig::validate` warns at startup.
- Episodic memory and embeddings are not yet wired (`fastembed` is declared but the write path is inert). Long-term retrieval uses word-overlap heuristics today.
- `--lto=thin` (LLVM) is out of scope for the current provider — KOD is a client, not a compiler.
- Some docs (`docs/SPEC.md`) describe a larger aspirational system than the workspace implements. That file is kept as a design reference; `docs/ARCHITECTURE.md` and `docs/TESTING.md` describe the code as it exists.

> [!WARNING]
> KOD is not a drop-in replacement for a full-featured IDE agent. It is a working harness whose goal is auditability and local-first control. Expect rough edges.

---

## License

MIT — see [`LICENSE`](LICENSE) for the full text. By contributing, you agree that your contributions will be licensed under the same terms.

---

<p align="center">
  <em>KOD is a work in progress. Contributions, bug reports, and design discussions are welcome.</em>
</p>
