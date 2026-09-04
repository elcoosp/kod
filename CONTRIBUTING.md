# Contributing to KOD

Thank you for your interest in contributing to KOD! This document provides guidelines and information for contributors.

## Development Setup

### Prerequisites
- Rust 1.75+ (install via [rustup](https://rustup.rs/))
- Git
- Ollama (for local LLM testing)

### Getting Started

1. **Fork and clone the repository:**
   ```bash
   git clone https://github.com/yourusername/kod.git
   cd kod
   ```

2. **Build the project:**
   ```bash
   cargo build
   ```

3. **Run tests:**
   ```bash
   cargo test --workspace
   ```

4. **Run the CLI:**
   ```bash
   cargo run -- status
   ```

## Development Workflow

### 1. Create a branch

```bash
git checkout -b feature/your-feature-name
# or
git checkout -b fix/your-bug-fix
```

### 2. Make changes

- Follow the code style guidelines below
- Add tests for new functionality
- Update documentation as needed
- Keep commits atomic and descriptive

### 3. Test your changes

```bash
# Format check
cargo fmt --all -- --check

# Lint
cargo clippy --workspace --all-targets -- -D warnings

# Tests
cargo test --workspace

# Build
cargo build --release
```

Or use the test script:
```bash
./scripts/test.sh
```

### 4. Submit a pull request

- Push your branch to your fork
- Create a pull request with a clear description
- Ensure all CI checks pass
- Wait for review

## Code Style Guidelines

### Rust Style

- Follow standard Rust formatting (`cargo fmt`)
- Use meaningful variable and function names
- Add docstrings for public items
- Prefer `Result<T, E>` over `Option<T>` for fallible operations
- Use `thiserror` for error definitions
- Keep functions focused and short

### Testing

- Write tests for all new functionality
- Use `#[tokio::test]` for async tests
- Test both success and failure paths
- Aim for >80% test coverage
- Use property-based testing where appropriate (proptest)

### Documentation

- Update README.md for user-facing changes
- Add docstrings for public APIs
- Include examples in documentation
- Keep documentation up-to-date with code changes

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

## Commit Message Guidelines

Use conventional commits:

```
<type>(<scope>): <subject>

<body>

<footer>
```

### Types
- `feat`: New feature
- `fix`: Bug fix
- `docs`: Documentation changes
- `style`: Code style changes (formatting, etc.)
- `refactor`: Code refactoring
- `test`: Test changes
- `chore`: Build or tooling changes

### Examples
```
feat(skills): add semantic matching support
fix(memory): resolve database corruption on concurrent access
docs(readme): update installation instructions
```

## Reporting Issues

When reporting issues, please include:

1. **Description** of the issue
2. **Steps to reproduce**
3. **Expected behavior**
4. **Actual behavior**
5. **Environment** (OS, Rust version, etc.)
6. **Additional context** (logs, screenshots, etc.)

## License

By contributing to KOD, you agree that your contributions will be licensed under the MIT License.
