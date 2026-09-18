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
├─────────────────────────────────────────────────────┤
│                LLM Providers                         │
│        (kod-provider, kod-provider-openai)           │
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

#### kod-provider-openai
OpenAI-compatible implementation (backed by `adk-model`):
- Single code path for Ollama (`/v1`), LM Studio, MLX Omni Serve, vLLM, OpenAI
- Automatic `/v1` base-URL normalization
- Generation (streaming and non-streaming) with tool calling
- Model listing via `GET /v1/models`

#### kod-provider-anthropic
Anthropic Messages API provider (backed by `adk-model`):
- The same `LlmProvider` trait as the OpenAI-compatible provider
- Wraps `adk_model::anthropic::Anthropic` — one code path per provider kind

#### kod-lsp
Minimal Language Server Protocol client for code intelligence:
- **Transport**: JSON-RPC over a server's stdio, `Content-Length` framing
- **Operations**: `initialize`, `didOpen`/`didChange`, `publishDiagnostics`, `definition`, `references`, `hover`
- **Lifecycle**: one server per language, spawned lazily, killed on engine shutdown

#### kod-mcp
Minimal Model Context Protocol client:
- **Transport**: newline-delimited JSON-RPC over a spawned server's stdio
- **Operations**: `initialize`, `tools/list`, `tools/call`
- **Registration**: tools surface on the engine's registry under the `mcp:<server>.<tool>` naming policy

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

## Delivered since this document was first written

- **LSP Integration** (`kod-lsp`): diagnostics, definition, references, hover.
- **Additional provider**: Anthropic Messages API (`kod-provider-anthropic`).
- **MCP client** (`kod-mcp`): external tools registered under the `mcp:<server>.<tool>` naming policy.
- **Sandbox backends**: bwrap on Linux, sandbox-exec on macOS, and Landlock on Linux kernels ≥ 5.13.
- **Unix-socket daemon** (`kod serve`): NDJSON protocol, peer-UID check, `--remote` on `kod prompt`, `kod chat`, `kod agent`.
- **Policy engine** (`kod-config::policy`): presets, per-tool overrides, session deny rules, `kod policy show|explain`.

## Still on the roadmap

- **Debugger Integration**: Debug Adapter Protocol for debugging.
- **Vector Search**: external vector databases (the current brute-force `VectorIndex` is in-process only).
- **Plugin System**: dynamic loading beyond MCP.
- **Prompt-plan migration**: the engine still renders the transcript
  to text at the `LlmProvider` boundary; AD-01/AD-16 replace that with
  a `CompletionRequest { system: SystemPrompt, messages: Vec<ChatMessage> }`
  so prompt caching and per-message role semantics are preserved on the
  wire. The types exist and are exercised by tests; the engine migration
  is the remaining step.
- **Anthropic `cache_control`**: `kod-provider-anthropic` currently
  delegates to `adk-model`'s Anthropic client, which flattens the system
  prompt to a single string before the wire call. Explicit cache
  breakpoints need either an `adk-model` API that accepts segments or a
  local wire module.
