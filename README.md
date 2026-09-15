# KOD

A high-performance AI coding agent for the terminal, built with Rust.

KOD is a multi-crate workspace that provides a full agent harness with skills,
memory, tool calling, and both CLI and TUI interfaces.

## Why local-first

Your code does not leave your machine.

This is the difference between KOD and every other terminal coding
agent. Claude Code, Aider, Crush, Goose — they are all designed around
a cloud model you do not control, on a schedule you do not set, under
terms that can change without your consent. When a vendor deprecates a
model or changes a rate limit, your workflow breaks. When the vendor
has a bad day, your tooling stops.

KOD is built around the opposite assumption:

- **The binary is one Rust program.** No Node runtime, no Python
  interpreter, no docker compose. `cargo build --release` produces a
  ~15 MB executable that runs the agent, the tools, the memory store,
  and the terminal UI.
- **The model is one you chose.** Ollama, LM Studio, MLX, vLLM, or any
  OpenAI-compatible endpoint — including OpenAI itself if you want
  cloud, but only if you want cloud. The default config points at
  `localhost:11434`.
- **Memory is on disk, in a file you can inspect.** Long-term memory
  lives in a `redb` database under `~/.kod/`, or per-project under
  `<cwd>/.kod/` if you set `memory.scope = "project"`. It is a file,
  not a tenant in someone else's index.
- **The tool loop is auditable.** `kod replay <session.jsonl>`
  re-runs the tool calls a session made, without the model. Every
  `read_file`, every `write_file`, every `execute_command` — checkable
  against the codebase at any later commit.

If that matters to you — if "where does my code go when I run this?"
is a question you want a one-line answer to — KOD is for you. If it
does not, the cloud agents are fine tools and you should use them.

KOD is not faster. It is not smarter. It is *yours*.

## Features

- **Skills System**: Markdown-based skills with pattern matching and hot reload
- **Memory System**: Multi-layer memory (short-term, long-term, episodic)
- **Tool Calling**: File system and git tools with permission-based sandboxing
- **LLM Provider Abstraction**: Pluggable provider support (Ollama built-in)
- **TUI Interface**: Full terminal UI with chat, agent panels, and input handling
- **CLI Interface**: Command-line interface for automation

## Installation

```bash
git clone https://github.com/kod-team/kod.git
cd kod
cargo build --release
```

## Usage

```bash
# Show help
./target/release/kod --help

# Run with verbose output (includes build info)
./target/release/kod --verbose config

# Start the TUI
cargo run -p kod-tui

# List available skills
cargo run -p kod-cli -- skills

# Show configuration
cargo run -p kod-cli -- config
```

## Development

### Building from Source

```bash
git clone https://github.com/kod-team/kod.git
cd kod
cargo build --release
```

### Running Tests

```bash
cargo test --workspace
./scripts/test.sh
```

### Running Benchmarks

```bash
cargo bench
```

### Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for guidelines.

### Changelog

See [CHANGELOG.md](CHANGELOG.md) for version history.

## Architecture

KOD uses a multi-crate workspace architecture with clear layer separation:

```
┌─────────────────────────────────────┐
│              CLI / TUI              │
│      (kod-cli, kod-tui)             │
├─────────────────────────────────────┤
│            Core Engine              │
│              (kod-core)              │
├───────┬────────┬────────┬──────────┤
│Skills │ Memory │  Core   │  Tools   │
├───────┴────────┴────────┴──────────┤
│           LLM Providers              │
│   (kod-provider, kod-provider-ollama) │
├─────────────────────────────────────┤
│             Foundation               │
│   (kod-types, kod-error, kod-config) │
└─────────────────────────────────────┘
```

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for detailed architecture documentation.

## Project Structure

```
kod/
├── crates/
│   ├── kod-types/           # Shared type definitions
│   ├── kod-error/           # Error definitions
│   ├── kod-config/          # Configuration
│   ├── kod-provider/        # LLM provider abstraction
│   ├── kod-provider-ollama/ # Ollama implementation
│   ├── kod-skills/          # Skills system
│   ├── kod-memory/          # Memory system
│   ├── kod-tools/           # Tool calling
│   ├── kod-tui/             # Terminal UI
│   ├── kod-cli/             # CLI interface
│   └── kod-core/            # Core engine
├── tests/                    # Integration tests
├── benches/                  # Benchmarks
├── docs/                     # Documentation
└── scripts/                  # Build/test scripts
```

## Configuration

KOD reads its configuration from `~/.kod/config.toml`. The config is auto-generated
on first run with sensible defaults.

## Skills

Skills are markdown files with YAML front matter. KOD discovers them in
these directories, in order; later directories shadow earlier ones for
skills that share a name:

1. `~/.kod/skills` — canonical KOD location
2. `~/.agents/skills` — Claude-style compatibility
3. `<project>/.kod/skills` — project-local KOD
4. `<project>/.agents/skills` — project-local Claude-style

Set `skills.skills_dir` in the config to use a single custom directory
instead. `kod skills` and `/skills` in the TUI both list the merged set.

## License

MIT License - see [LICENSE](LICENSE) file for details.
