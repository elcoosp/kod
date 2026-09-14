# KOD

A high-performance AI coding agent for the terminal, built with Rust.

KOD is a multi-crate workspace that provides a full agent harness with skills,
memory, agent swarms, tool calling, and both CLI and TUI interfaces.

## Features

- **Skills System**: Markdown-based skills with pattern matching and hot reload
- **Memory System**: Multi-layer memory (short-term, long-term, episodic)
- **Agent Swarm**: Multi-agent coordination with task decomposition
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
│Skills │ Memory │  Swarm  │  Tools   │
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
│   ├── kod-swarm/           # Agent swarm
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
