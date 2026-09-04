# KOD Architecture

## Overview

KOD is a high-performance AI coding agent harness built with a multi-crate workspace architecture. The system is designed to be modular, extensible, and performant.

## High-Level Architecture

```
┌─────────────────────────────────────────────────────┐
│                    CLI / TUI                         │
│           (kod-cli, kod-tui)                        │
├─────────────────────────────────────────────────────┤
│                   Core Engine                        │
│                      (kod-core)                     │
├─────────────┬─────────────┬─────────────┬──────────┤
│   Skills    │   Memory    │   Swarm     │  Tools   │
│ (kod-skills)│(kod-memory) │ (kod-swarm) │(kod-tools)│
├─────────────┴─────────────┴─────────────┴──────────┤
│                LLM Providers                         │
│        (kod-provider, kod-provider-ollama)           │
├─────────────────────────────────────────────────────┤
│                   Foundation                         │
│        (kod-types, kod-error, kod-config)            │
└─────────────────────────────────────────────────────┘
```

## Crate Structure

### Foundation Layer

#### kod-types
Shared type definitions used across all crates:
- Strongly-typed IDs (AgentId, MessageId, SkillId, etc.)
- Message types (ChatMessage, AgentMessage)
- Skill definitions and metadata
- Memory types and entries
- Tool definitions and calls

#### kod-error
Centralized error handling:
- Comprehensive error enum covering all error cases
- Recovery detection (recoverable vs non-recoverable)
- User-friendly error messages

#### kod-config
Configuration management:
- TOML-based configuration
- Environment variable support
- Default value handling
- Configuration validation

### LLM Provider Layer

#### kod-provider
Provider abstraction:
- `LlmProvider` trait for LLM integration
- Generation options and responses
- Streaming support

#### kod-provider-ollama
Ollama implementation:
- HTTP client with health checking
- Generation (streaming and non-streaming)
- Tool calling support
- Model management

### Skills Layer

#### kod-skills
Markdown-based skill system:
- **Parser**: YAML front matter and markdown parsing
- **Loader**: Directory scanning and caching
- **Matcher**: Pattern-based skill matching
- **Watcher**: File system watching for hot reload

### Memory Layer

#### kod-memory
Multi-layer memory system:
- **Short-term**: In-memory with capacity limits
- **Long-term**: Persistent (redb) storage
- **Episodic**: Vector-based for semantic search
- **Manager**: Unified interface
- **Context**: Context builder for LLM prompts

### Tool Layer

#### kod-tools
Tool calling system:
- **Registry**: Tool management and discovery
- **Context**: Execution context with permissions
- **Executor**: Tool execution with timeout
- **Tools**: Built-in tools (file system, git)

### Swarm Layer

#### kod-swarm
Agent swarm coordination:
- **Agent**: Agent lifecycle and capabilities
- **Communication**: Direct messaging between agents
- **Workspace**: Shared workspace with file locking
- **Coordination**: Task decomposition and assignment
- **Swarm**: Swarm manager

### Interface Layer

#### kod-tui
Terminal UI:
- **Event**: Event handling system
- **App**: Application state
- **UI**: Widgets (chat, agent panel, input)
- **Main Loop**: Rendering and event coordination

#### kod-cli
Command-line interface:
- **Commands**: CLI structure with clap
- **Handlers**: Command handlers
- **Main**: Entry point

### Core Layer

#### kod-core
Core engine:
- **Router**: Task classification and routing
- **Engine**: Main engine orchestration
- **Context**: Engine context management
- **Config**: Engine configuration

## Data Flow

1. **User Input** → CLI/TUI
2. **Task Classification** → Core Engine
3. **Context Building** → Memory + Skills
4. **Task Routing** → Core Engine
5. **LLM Generation** → Provider
6. **Tool Execution** → Tools (if needed)
7. **Swarm Coordination** → Swarm (if needed)
8. **Response** → CLI/TUI

## Design Principles

1. **Modularity**: Each crate has a single responsibility
2. **Performance**: Zero-copy parsing, object pooling, caching
3. **Extensibility**: Trait-based abstractions for providers and tools
4. **Type Safety**: Strongly-typed IDs prevent mixing
5. **Error Handling**: Comprehensive error types with recovery
6. **Testing**: High test coverage with property-based testing
7. **Documentation**: Comprehensive documentation for all components

## Performance Considerations

- **Memory efficiency**: Object pooling, string interning
- **Async I/O**: Tokio-based async runtime
- **Caching**: Multi-level caching for skills and memory
- **Zero-copy parsing**: Minimize allocations where possible
- **Connection pooling**: HTTP connection reuse

## Future Considerations

- **LSP Integration**: Language Server Protocol for code intelligence
- **Debugger Integration**: Debug Adapter Protocol for debugging
- **Additional Providers**: Anthropic, OpenAI, custom endpoints
- **Vector Search**: Integration with vector databases
- **Plugin System**: Dynamic loading of plugins
- **Remote Swarm**: Distributed agent coordination
