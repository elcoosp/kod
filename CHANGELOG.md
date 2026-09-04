# Changelog

All notable changes to KOD will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Initial release with core functionality
- Skills system with markdown-based skills
- Memory system (short-term, long-term, episodic)
- Agent swarm with coordination
- Tool calling system
- TUI interface
- CLI interface
- Ollama LLM provider support

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
