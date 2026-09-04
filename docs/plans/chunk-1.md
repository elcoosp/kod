I'm using the writing-plans skill to create the implementation plan.

# KOD Implementation Plan

**Goal:** Build a high-performance, terminal-native AI coding agent harness with multi-crate workspace architecture, featuring skills system, agent swarms, and LLM integration.

**Architecture:** Multi-crate Cargo workspace with isolated concerns: core types, error handling, configuration, LLM providers, skills, memory, tools, agent swarm, and TUI. Each crate has minimal dependencies and clear boundaries.

**Tech Stack:** Rust, Tokio, Ratatui, redb, fastembed, Ollama, reqwest, notify

---

## File Structure Overview

```
kod/
├── Cargo.toml                    # Workspace root
├── crates/
│   ├── kod-types/               # Shared type definitions
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── ids.rs           # Strongly-typed IDs
│   │       ├── message.rs       # Message types
│   │       ├── skill.rs         # Skill types
│   │       ├── memory.rs        # Memory types
│   │       └── tool.rs          # Tool types
│   │
│   ├── kod-error/               # Error definitions
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       └── error.rs         # KodError enum
│   │
│   ├── kod-config/              # Configuration
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── config.rs        # Main config
│   │       ├── llm.rs           # LLM configuration
│   │       ├── swarm.rs         # Swarm configuration
│   │       ├── memory.rs        # Memory configuration
│   │       ├── skills.rs        # Skills configuration
│   │       └── ui.rs            # UI configuration
│   │
│   ├── kod-provider/            # LLM provider abstraction
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── traits.rs        # Provider trait
│   │       ├── types.rs         # Generation types
│   │       └── streaming.rs     # SSE parsing
│   │
│   ├── kod-provider-ollama/     # Ollama implementation
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── client.rs        # HTTP client
│   │       ├── generate.rs      # Generation endpoint
│   │       └── models.rs        # Model management
│   │
│   ├── kod-skills/              # Skills system
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── loader.rs        # Skill file loading
│   │       ├── parser.rs        # Markdown parsing
│   │       ├── matcher.rs       # Skill matching
│   │       └── watcher.rs       # Hot reloading
│   │
│   ├── kod-memory/              # Memory system
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── manager.rs       # Memory manager
│   │       ├── short_term.rs    # In-memory storage
│   │       ├── long_term.rs     # redb storage
│   │       └── episodic.rs      # Vector storage
│   │
│   ├── kod-tools/               # Tool calling
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── registry.rs      # Tool registry
│   │       ├── executor.rs      # Tool execution
│   │       └── tools/
│   │           ├── mod.rs
│   │           ├── file_read.rs
│   │           ├── file_write.rs
│   │           └── git_status.rs
│   │
│   ├── kod-swarm/               # Agent swarm
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── agent.rs         # Agent definition
│   │       ├── communication.rs # Messaging hub
│   │       ├── workspace.rs     # Shared workspace
│   │       └── coordination.rs  # Task coordination
│   │
│   ├── kod-tui/                 # Terminal UI
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── app.rs           # Main app state
│   │       ├── event.rs         # Event handling
│   │       ├── ui/
│   │       │   ├── mod.rs
│   │       │   ├── chat.rs      # Chat display
│   │       │   ├── agents.rs    # Agent panel
│   │       │   └── input.rs     # Input area
│   │       └── render.rs        # Rendering logic
│   │
│   ├── kod-cli/                 # CLI interface
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── commands.rs      # Command definitions
│   │       └── handlers.rs      # Command handlers
│   │
│   └── kod-core/                # Core orchestration
│       ├── Cargo.toml
│       └── src/
│           ├── lib.rs
│           ├── router.rs        # Task router
│           ├── context.rs       # Context builder
│           └── engine.rs        # Main engine
│
├── tests/                       # Integration tests
│   └── common/
│       └── mod.rs
│
└── docs/
    └── superpowers/
        └── plans/
            └── 2026-01-07-kod-implementation.md  # This plan
```

---

## Chunk 1: Workspace Foundation & Core Types

### Task 1: Initialize Workspace

**Files:**
- Create: `Cargo.toml`
- Create: `.gitignore`
- Create: `rust-toolchain.toml`

- [ ] **Step 1: Create workspace Cargo.toml**

```toml
[workspace]
resolver = "2"
members = [
    "crates/kod-types",
    "crates/kod-error",
    "crates/kod-config",
    "crates/kod-provider",
    "crates/kod-provider-ollama",
    "crates/kod-skills",
    "crates/kod-memory",
    "crates/kod-tools",
    "crates/kod-swarm",
    "crates/kod-tui",
    "crates/kod-cli",
    "crates/kod-core",
]

[workspace.package]
version = "0.1.0"
edition = "2021"
license = "MIT"
authors = ["KOD Team"]

[workspace.dependencies]
# Core
serde = { version = "1.0.229", features = ["derive"] }
serde_json = "1.0.151"
thiserror = "2.0.19"
anyhow = "1.0.104"
bytes = "1.11.1"

# Async
tokio = { version = "1.52.3", features = ["rt", "rt-multi-thread", "sync", "time", "macros", "process", "net", "io-util", "io-std"] }
reqwest = { version = "0.13.4", default-features = false, features = ["json", "rustls"] }
eventsource-stream = "0.2.3"
futures = "0.3.31"
async-stream = "0.3"

# Data structures
compact_str = { version = "0.10.0", features = ["serde"] }
bitflags = { version = "2.13.0", features = ["serde"] }
slotmap = "1.1.1"
arc-swap = "1.9.2"
parking_lot = "0.12.5"

# Storage
redb = "4.1.0"

# Time & dirs
time = { version = "0.3.55", features = ["serde", "formatting", "parsing"] }
dirs = "6.0.0"

# CLI & config
clap = { version = "4.6.6", features = ["derive", "env"] }
toml = "1.1.2"

# Git & GitHub
octocrab = "0.54.1"

# Tools & schema
schemars = "1.2.2"
walkdir = "2.5.0"
globset = "0.4.20"
notify = "8.2.0"
fs4 = "1.1.0"

# Tracing
tracing = "0.1.44"
tracing-subscriber = { version = "0.3.23", features = ["env-filter", "json", "fmt"] }

# Embedding (heavy — isolated in kod-embed)
fastembed = "5.17.4"

# TUI
ratatui = "0.29.0"
crossterm = { version = "0.28.1", features = ["event-stream"] }

# Testing
proptest = "1.11.0"
rstest = "0.26.1"

[profile.release]
lto = true
codegen-units = 1
panic = "abort"
strip = true

[profile.dev]
opt-level = 1
```

- [ ] **Step 2: Create .gitignore**

```gitignore
/target
Cargo.lock
*.swp
*.swo
.DS_Store
.env
.env.local
*.log
```

- [ ] **Step 3: Create rust-toolchain.toml**

```toml
[toolchain]
channel = "stable"
components = ["rustfmt", "clippy"]
```

- [ ] **Step 4: Initialize git repository**

```bash
git init
git add Cargo.toml .gitignore rust-toolchain.toml
git commit -m "chore: initialize workspace with core dependencies"
```

---

### Task 2: Create kod-types Crate

**Files:**
- Create: `crates/kod-types/Cargo.toml`
- Create: `crates/kod-types/src/lib.rs`
- Create: `crates/kod-types/src/ids.rs`
- Create: `crates/kod-types/src/message.rs`
- Create: `crates/kod-types/src/skill.rs`
- Create: `crates/kod-types/src/memory.rs`
- Create: `crates/kod-types/src/tool.rs`

- [ ] **Step 1: Create crate Cargo.toml**

```toml
[package]
name = "kod-types"
version.workspace = true
edition.workspace = true
license.workspace = true
authors.workspace = true

[dependencies]
serde = { workspace = true }
serde_json = { workspace = true }
compact_str = { workspace = true }
time = { workspace = true }
uuid = { version = "1.11", features = ["v4", "serde"] }

[dev-dependencies]
rstest = { workspace = true }
```

- [ ] **Step 2: Write failing test for ID types**

Create `crates/kod-types/src/lib.rs`:

```rust
pub mod ids;
pub mod message;
pub mod skill;
pub mod memory;
pub mod tool;

pub use ids::*;
pub use message::*;
pub use skill::*;
pub use memory::*;
pub use tool::*;
```

Create `crates/kod-types/src/ids.rs`:

```rust
//! Strongly-typed identifiers to prevent mixing IDs across domains.
//!
//! These types use UUIDs internally but expose type-safe wrappers to prevent
//! accidentally using an agent ID where a message ID is expected.

use serde::{Deserialize, Serialize};
use std::fmt;

macro_rules! define_id {
    ($name:ident, $prefix:literal) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
        pub struct $name(pub uuid::Uuid);

        impl $name {
            /// Create a new unique identifier
            pub fn new() -> Self {
                Self(uuid::Uuid::new_v4())
            }

            /// Create from an existing UUID
            pub fn from_uuid(id: uuid::Uuid) -> Self {
                Self(id)
            }

            /// Get the underlying UUID
            pub fn as_uuid(&self) -> &uuid::Uuid {
                &self.0
            }

            /// Get string representation with prefix
            pub fn to_prefixed_string(&self) -> String {
                format!("{}-{}", $prefix, self.0)
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}-{}", $prefix, &self.0.to_string()[..8])
            }
        }

        impl std::str::FromStr for $name {
            type Err = uuid::Error;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                // Strip prefix if present
                let uuid_str = s.strip_prefix(concat!($prefix, "-")).unwrap_or(s);
                Ok(Self(uuid::Uuid::parse_str(uuid_str)?))
            }
        }
    };
}

define_id!(AgentId, "agent");
define_id!(MessageId, "msg");
define_id!(SkillId, "skill");
define_id!(MemoryId, "mem");
define_id!(ToolId, "tool");
define_id!(TaskId, "task");
define_id!(SessionId, "session");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_agent_id_creation() {
        let id = AgentId::new();
        assert!(!id.as_uuid().is_nil());
    }

    #[test]
    fn test_id_display() {
        let id = AgentId::new();
        let display = id.to_string();
        assert!(display.starts_with("agent-"));
        assert_eq!(display.len(), 14); // "agent-" + 8 chars
    }

    #[test]
    fn test_id_from_str() {
        let id = AgentId::new();
        let string = id.to_prefixed_string();
        let parsed: AgentId = string.parse().unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn test_different_id_types_not_equal() {
        let agent_id = AgentId::new();
        let message_id = MessageId::new();
        
        // This should not compile if uncommented:
        // assert_eq!(agent_id, message_id);
        
        // But their UUIDs can be compared
        assert_ne!(agent_id.as_uuid(), message_id.as_uuid());
    }
}
```

- [ ] **Step 3: Run tests to verify they pass**

```bash
cargo test -p kod-types
```

Expected: All tests pass

- [ ] **Step 4: Create message types**

Create `crates/kod-types/src/message.rs`:

```rust
//! Message types for communication between agents, users, and the system.

use crate::ids::{AgentId, MessageId};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub id: MessageId,
    pub role: MessageRole,
    pub content: String,
    pub timestamp: OffsetDateTime,
    pub metadata: MessageMetadata,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MessageRole {
    User,
    Assistant,
    System,
    Tool,
    Agent(AgentId),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MessageMetadata {
    pub skill_applied: Option<String>,
    pub tools_used: Vec<String>,
    pub agent_id: Option<AgentId>,
    pub thinking_time_ms: Option<u64>,
    pub token_count: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentMessage {
    pub id: MessageId,
    pub from: AgentId,
    pub to: MessageDestination,
    pub content: AgentMessageContent,
    pub timestamp: OffsetDateTime,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MessageDestination {
    Agent(AgentId),
    Broadcast,
    Coordinator,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentMessageContent {
    TaskAssignment {
        description: String,
        priority: Priority,
    },
    ProgressUpdate {
        status: TaskStatus,
        details: String,
    },
    HelpRequest {
        question: String,
        context: String,
    },
    KnowledgeShare {
        information: String,
        tags: Vec<String>,
    },
    Coordination {
        action: CoordinationAction,
    },
    FileClaim {
        path: String,
        duration_secs: u64,
    },
    FileRelease {
        path: String,
    },
    ResultDelivery {
        result: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Priority {
    Low,
    Medium,
    High,
    Critical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskStatus {
    Pending,
    InProgress,
    Blocked,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CoordinationAction {
    RequestingSync,
    ProposingChange {
        file: String,
        description: String,
    },
    AcknowledgingChange {
        file: String,
    },
    ConflictDetected {
        file: String,
        description: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chat_message_serialization() {
        let msg = ChatMessage {
            id: MessageId::new(),
            role: MessageRole::User,
            content: "Hello, world".to_string(),
            timestamp: OffsetDateTime::now_utc(),
            metadata: MessageMetadata::default(),
        };

        let json = serde_json::to_string(&msg).unwrap();
        let deserialized: ChatMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(msg.id, deserialized.id);
        assert_eq!(msg.content, deserialized.content);
    }

    #[test]
    fn test_agent_message_serialization() {
        let msg = AgentMessage {
            id: MessageId::new(),
            from: AgentId::new(),
            to: MessageDestination::Broadcast,
            content: AgentMessageContent::TaskAssignment {
                description: "Implement feature".to_string(),
                priority: Priority::High,
            },
            timestamp: OffsetDateTime::now_utc(),
        };

        let json = serde_json::to_string(&msg).unwrap();
        let deserialized: AgentMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(msg.id, deserialized.id);
    }
}
```

- [ ] **Step 5: Run tests for message types**

```bash
cargo test -p kod-types -- message
```

Expected: All message tests pass

- [ ] **Step 6: Create skill types**

Create `crates/kod-types/src/skill.rs`:

```rust
//! Skill definitions for the markdown-based skill system.

use crate::ids::SkillId;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    pub id: SkillId,
    pub metadata: SkillMetadata,
    pub instructions: String,
    pub examples: Vec<SkillExample>,
    pub constraints: Option<String>,
    pub content: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillMetadata {
    pub name: String,
    pub description: String,
    pub version: String,
    pub author: Option<String>,
    pub category: String,
    pub tags: Vec<String>,
    pub capabilities: Vec<String>,
    pub requirements: Vec<String>,
    pub triggers: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillExample {
    pub input: String,
    pub output: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillMatch {
    pub skill: Skill,
    pub score: f32,
    pub match_reasons: Vec<MatchReason>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MatchReason {
    TriggerMatch { trigger: String },
    TagMatch { tag: String },
    CapabilityMatch { capability: String },
    SemanticSimilarity { score: f32 },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_skill_serialization() {
        let skill = Skill {
            id: SkillId::new(),
            metadata: SkillMetadata {
                name: "rust-refactoring".to_string(),
                description: "Rust code refactoring".to_string(),
                version: "1.0.0".to_string(),
                author: None,
                category: "coding".to_string(),
                tags: vec!["rust".to_string()],
                capabilities: vec!["refactoring".to_string()],
                requirements: vec![],
                triggers: vec!["refactor rust".to_string()],
            },
            instructions: "You are a Rust expert.".to_string(),
            examples: vec![],
            constraints: None,
            content: "Full content".to_string(),
            path: PathBuf::from("/skills/rust.md"),
        };

        let json = serde_json::to_string(&skill).unwrap();
        let deserialized: Skill = serde_json::from_str(&json).unwrap();
        assert_eq!(skill.metadata.name, deserialized.metadata.name);
    }
}
```

- [ ] **Step 7: Run tests for skill types**

```bash
cargo test -p kod-types -- skill
```

Expected: All skill tests pass

- [ ] **Step 8: Create memory types**

Create `crates/kod-types/src/memory.rs`:

```rust
//! Memory types for the multi-layer memory system.

use crate::ids::{MemoryId, SessionId};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MemoryType {
    ShortTerm,
    LongTerm,
    Episodic,
    Semantic,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub id: MemoryId,
    pub memory_type: MemoryType,
    pub content: String,
    pub timestamp: OffsetDateTime,
    pub relevance: f32,
    pub metadata: MemoryMetadata,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MemoryMetadata {
    pub session_id: Option<SessionId>,
    pub tags: Vec<String>,
    pub embedding: Option<Vec<f32>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MemoryContext {
    pub working_memory: Vec<MemoryEntry>,
    pub long_term: Vec<MemoryEntry>,
    pub episodic: Vec<EpisodicMemory>,
    pub total_tokens: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EpisodicMemory {
    pub id: MemoryId,
    pub content: String,
    pub embedding: Vec<f32>,
    pub task_type: String,
    pub outcome: Outcome,
    pub timestamp: OffsetDateTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Outcome {
    Success,
    Partial,
    Failure,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_memory_entry_serialization() {
        let entry = MemoryEntry {
            id: MemoryId::new(),
            memory_type: MemoryType::ShortTerm,
            content: "User prefers dark mode".to_string(),
            timestamp: OffsetDateTime::now_utc(),
            relevance: 0.9,
            metadata: MemoryMetadata::default(),
        };

        let json = serde_json::to_string(&entry).unwrap();
        let deserialized: MemoryEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry.id, deserialized.id);
    }
}
```

- [ ] **Step 9: Run tests for memory types**

```bash
cargo test -p kod-types -- memory
```

Expected: All memory tests pass

- [ ] **Step 10: Create tool types**

Create `crates/kod-types/src/tool.rs`:

```rust
//! Tool definitions and calling types.

use crate::ids::ToolId;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub id: ToolId,
    pub name: String,
    pub description: String,
    pub category: ToolCategory,
    pub parameters_schema: Value,
    pub permissions: ToolPermissions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ToolCategory {
    FileSystem,
    Git,
    Web,
    Code,
    System,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolPermissions {
    pub read_files: bool,
    pub write_files: bool,
    pub execute_commands: bool,
    pub network_access: bool,
    pub git_operations: bool,
    pub allowed_paths: Vec<String>,
    pub forbidden_paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub tool_name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ToolResult {
    Success(Value),
    Error(String),
    RequiresConfirmation {
        description: String,
        callback_id: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolExecution {
    pub call: ToolCall,
    pub result: ToolResult,
    pub execution_time_ms: u64,
    pub timestamp: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_call_serialization() {
        let call = ToolCall {
            tool_name: "read_file".to_string(),
            arguments: serde_json::json!({"path": "/test.rs"}),
        };

        let json = serde_json::to_string(&call).unwrap();
        let deserialized: ToolCall = serde_json::from_str(&json).unwrap();
        assert_eq!(call.tool_name, deserialized.tool_name);
    }
}
```

- [ ] **Step 11: Run all kod-types tests**

```bash
cargo test -p kod-types
```

Expected: All tests pass

- [ ] **Step 12: Commit kod-types**

```bash
git add crates/kod-types/
git commit -m "feat(kod-types): add core type definitions with strongly-typed IDs"
```

---

### Task 3: Create kod-error Crate

**Files:**
- Create: `crates/kod-error/Cargo.toml`
- Create: `crates/kod-error/src/lib.rs`
- Create: `crates/kod-error/src/error.rs`

- [ ] **Step 1: Create crate Cargo.toml**

```toml
[package]
name = "kod-error"
version.workspace = true
edition.workspace = true
license.workspace = true
authors.workspace = true

[dependencies]
thiserror = { workspace = true }
kod-types = { path = "../kod-types" }
```

- [ ] **Step 2: Write failing test for error types**

Create `crates/kod-error/src/lib.rs`:

```rust
pub mod error;

pub use error::KodError;
pub type Result<T> = std::result::Result<T, KodError>;
```

Create `crates/kod-error/src/error.rs`:

```rust
//! Comprehensive error types for the KOD system.

use thiserror::Error;
use kod_types::{AgentId, SkillId};

#[derive(Error, Debug)]
pub enum KodError {
    // Core errors
    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Invalid state: {0}")]
    InvalidState(String),

    // LLM Provider errors
    #[error("Provider error: {0}")]
    Provider(String),

    #[error("Provider timeout after {timeout_ms}ms")]
    ProviderTimeout { timeout_ms: u64 },

    #[error("Rate limited by provider, retry after {retry_after_secs}s")]
    RateLimited { retry_after_secs: u64 },

    #[error("Model not found: {model}")]
    ModelNotFound { model: String },

    // Skill errors
    #[error("Skill not found: {skill_id}")]
    SkillNotFound { skill_id: SkillId },

    #[error("Skill parse error in {path}: {reason}")]
    SkillParseError { path: String, reason: String },

    #[error("Skill validation failed: {reason}")]
    SkillValidationFailed { reason: String },

    // Memory errors
    #[error("Memory storage error: {0}")]
    MemoryStorage(String),

    #[error("Memory database error: {0}")]
    MemoryDatabase(String),

    #[error("Embedding generation failed: {0}")]
    EmbeddingGeneration(String),

    // Agent Swarm errors
    #[error("Agent not found: {agent_id}")]
    AgentNotFound { agent_id: AgentId },

    #[error("Agent communication error: {0}")]
    AgentCommunication(String),

    #[error("Swarm coordination failed: {0}")]
    SwarmCoordination(String),

    #[error("File lock timeout for {path}")]
    LockTimeout { path: String },

    #[error("Conflict detected in {path}: {description}")]
    ConflictDetected { path: String, description: String },

    // Tool errors
    #[error("Tool not found: {tool_name}")]
    ToolNotFound { tool_name: String },

    #[error("Tool execution failed: {tool_name}: {reason}")]
    ToolExecution { tool_name: String, reason: String },

    #[error("Permission denied for {action}: {reason}")]
    PermissionDenied { action: String, reason: String },

    #[error("Invalid tool parameters: {reason}")]
    InvalidParameters { reason: String },

    // File system errors
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("File not found: {path}")]
    FileNotFound { path: String },

    #[error("File modified since edit was created: {path}")]
    FileModifiedSinceEdit { path: String },

    // Serialization errors
    #[error("Serialization error: {0}")]
    Serialization(String),

    #[error("Deserialization error: {0}")]
    Deserialization(String),

    // Network errors
    #[error("Network error: {0}")]
    Network(String),

    // Sandbox violations
    #[error("Sandbox violation: {0}")]
    SandboxViolation(String),

    // Internal errors
    #[error("Internal error: {0}")]
    Internal(String),
}

impl KodError {
    /// Check if this error is recoverable (can retry)
    pub fn is_recoverable(&self) -> bool {
        matches!(
            self,
            KodError::ProviderTimeout { .. }
                | KodError::RateLimited { .. }
                | KodError::Network(_)
                | KodError::LockTimeout { .. }
        )
    }

    /// Get a user-friendly error message
    pub fn user_message(&self) -> String {
        match self {
            KodError::Provider(msg) => format!("The AI provider encountered an issue: {}", msg),
            KodError::SkillNotFound { skill_id } => {
                format!("The skill '{}' could not be found", skill_id)
            }
            KodError::AgentNotFound { agent_id } => {
                format!("The agent '{}' is not available", agent_id)
            }
            KodError::PermissionDenied { action, reason } => {
                format!("Permission denied for '{}': {}", action, reason)
            }
            _ => self.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_display() {
        let error = KodError::SkillNotFound {
            skill_id: kod_types::SkillId::new(),
        };
        assert!(error.to_string().contains("Skill not found"));
    }

    #[test]
    fn test_recoverable_errors() {
        assert!(KodError::ProviderTimeout { timeout_ms: 1000 }.is_recoverable());
        assert!(KodError::RateLimited { retry_after_secs: 30 }.is_recoverable());
        assert!(!KodError::Internal("test".to_string()).is_recoverable());
    }

    #[test]
    fn test_user_message() {
        let error = KodError::PermissionDenied {
            action: "write_file".to_string(),
            reason: "path not allowed".to_string(),
        };
        let message = error.user_message();
        assert!(message.contains("Permission denied"));
    }
}
```

- [ ] **Step 3: Run kod-error tests**

```bash
cargo test -p kod-error
```

Expected: All tests pass

- [ ] **Step 4: Commit kod-error**

```bash
git add crates/kod-error/
git commit -m "feat(kod-error): add comprehensive error types with recovery detection"
```

---

### Task 4: Create kod-config Crate

**Files:**
- Create: `crates/kod-config/Cargo.toml`
- Create: `crates/kod-config/src/lib.rs`
- Create: `crates/kod-config/src/config.rs`
- Create: `crates/kod-config/src/llm.rs`
- Create: `crates/kod-config/src/swarm.rs`
- Create: `crates/kod-config/src/memory.rs`
- Create: `crates/kod-config/src/skills.rs`

- [ ] **Step 1: Create crate Cargo.toml**

```toml
[package]
name = "kod-config"
version.workspace = true
edition.workspace = true
license.workspace = true
authors.workspace = true

[dependencies]
serde = { workspace = true }
toml = { workspace = true }
dirs = { workspace = true }
kod-error = { path = "../kod-error" }

[dev-dependencies]
rstest = { workspace = true }
```

- [ ] **Step 2: Write failing test for config loading**

Create `crates/kod-config/src/lib.rs`:

```rust
pub mod config;
pub mod llm;
pub mod swarm;
pub mod memory;
pub mod skills;

pub use config::KodConfig;
pub use llm::LlmConfig;
pub use swarm::SwarmConfig;
pub use memory::MemoryConfig;
pub use skills::SkillsConfig;
```

Create `crates/kod-config/src/config.rs`:

```rust
//! Main configuration for KOD.

use crate::{LlmConfig, MemoryConfig, SkillsConfig, SwarmConfig};
use kod_error::{KodError, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct KodConfig {
    pub llm: LlmConfig,
    pub swarm: SwarmConfig,
    pub memory: MemoryConfig,
    pub skills: SkillsConfig,
    pub performance: PerformanceConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerformanceConfig {
    pub max_memory_mb: usize,
    pub target_response_time_ms: u64,
    pub enable_object_pooling: bool,
}

impl Default for PerformanceConfig {
    fn default() -> Self {
        Self {
            max_memory_mb: 150,
            target_response_time_ms: 200,
            enable_object_pooling: true,
        }
    }
}

impl KodConfig {
    /// Load configuration from the default location (~/.kod/config.toml)
    pub fn load_default() -> Result<Self> {
        let config_dir = Self::config_dir()?;
        let config_path = config_dir.join("config.toml");
        
        if config_path.exists() {
            Self::load_from(&config_path)
        } else {
            // Create default config
            let config = Self::default();
            config.save_to(&config_path)?;
            Ok(config)
        }
    }

    /// Load configuration from a specific path
    pub fn load_from(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| KodError::Config(format!("Failed to read config file: {}", e)))?;
        
        let config: KodConfig = toml::from_str(&content)
            .map_err(|e| KodError::Config(format!("Failed to parse config: {}", e)))?;
        
        Ok(config)
    }

    /// Save configuration to a specific path
    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| KodError::Config(format!("Failed to create config dir: {}", e)))?;
        }
        
        let content = toml::to_string_pretty(self)
            .map_err(|e| KodError::Config(format!("Failed to serialize config: {}", e)))?;
        
        std::fs::write(path, content)
            .map_err(|e| KodError::Config(format!("Failed to write config: {}", e)))?;
        
        Ok(())
    }

    /// Get the configuration directory
    pub fn config_dir() -> Result<PathBuf> {
        dirs::config_dir()
            .map(|d| d.join("kod"))
            .ok_or_else(|| KodError::Config("Could not determine config directory".to_string()))
    }

    /// Get the skills directory
    pub fn skills_dir(&self) -> Result<PathBuf> {
        if self.skills.skills_dir.is_some() {
            Ok(PathBuf::from(self.skills.skills_dir.as_ref().unwrap()))
        } else {
            dirs::home_dir()
                .map(|h| h.join(".kod").join("skills"))
                .ok_or_else(|| KodError::Config("Could not determine home directory".to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_default_config() {
        let config = KodConfig::default();
        assert_eq!(config.llm.model, "codellama:13b");
        assert_eq!(config.swarm.max_agents, 5);
    }

    #[test]
    fn test_config_serialization_roundtrip() {
        let config = KodConfig::default();
        let toml_str = toml::to_string(&config).unwrap();
        let deserialized: KodConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(config.llm.model, deserialized.llm.model);
    }

    #[test]
    fn test_config_load_from_file() {
        let temp_dir = tempfile::tempdir().unwrap();
        let config_path = temp_dir.path().join("config.toml");
        
        let config = KodConfig::default();
        config.save_to(&config_path).unwrap();
        
        let loaded = KodConfig::load_from(&config_path).unwrap();
        assert_eq!(config.llm.model, loaded.llm.model);
    }

    #[test]
    fn test_invalid_config_file() {
        let temp_dir = tempfile::tempdir().unwrap();
        let config_path = temp_dir.path().join("config.toml");
        
        let mut file = std::fs::File::create(&config_path).unwrap();
        writeln!(file, "invalid toml [").unwrap();
        
        let result = KodConfig::load_from(&config_path);
        assert!(result.is_err());
    }
}
```

- [ ] **Step 3: Create LLM configuration**

Create `crates/kod-config/src/llm.rs`:

```rust
//! LLM provider configuration.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmConfig {
    pub provider: ProviderType,
    pub model: String,
    pub base_url: String,
    pub api_key: Option<String>,
    pub context_window: usize,
    pub max_tokens: usize,
    pub temperature: f32,
    pub timeout_secs: u64,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            provider: ProviderType::Ollama,
            model: "codellama:13b".to_string(),
            base_url: "http://localhost:11434".to_string(),
            api_key: None,
            context_window: 8192,
            max_tokens: 2048,
            temperature: 0.7,
            timeout_secs: 300,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ProviderType {
    Ollama,
    Anthropic,
    OpenAI,
    Custom,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_llm_config() {
        let config = LlmConfig::default();
        assert_eq!(config.provider, ProviderType::Ollama);
        assert_eq!(config.model, "codellama:13b");
        assert_eq!(config.base_url, "http://localhost:11434");
    }
}
```

- [ ] **Step 4: Create swarm configuration**

Create `crates/kod-config/src/swarm.rs`:

```rust
//! Agent swarm configuration.

use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwarmConfig {
    pub default_mode: CollaborationMode,
    pub max_agents: usize,
    pub coordination_strategy: CoordinationStrategy,
    pub lock_timeout_secs: u64,
    pub agent_idle_timeout_secs: u64,
}

impl Default for SwarmConfig {
    fn default() -> Self {
        Self {
            default_mode: CollaborationMode::SharedBranch,
            max_agents: 5,
            coordination_strategy: CoordinationStrategy::AgentDecided,
            lock_timeout_secs: 30,
            agent_idle_timeout_secs: 300,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CollaborationMode {
    SharedBranch,
    IsolatedWorktrees,
    Hybrid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CoordinationStrategy {
    LockBased,
    SemanticCoordination,
    AgentDecided,
}

impl SwarmConfig {
    pub fn lock_timeout(&self) -> Duration {
        Duration::from_secs(self.lock_timeout_secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_swarm_config() {
        let config = SwarmConfig::default();
        assert_eq!(config.default_mode, CollaborationMode::SharedBranch);
        assert_eq!(config.max_agents, 5);
    }
}
```

- [ ] **Step 5: Create memory configuration**

Create `crates/kod-config/src/memory.rs`:

```rust
//! Memory system configuration.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryConfig {
    pub short_term_capacity: usize,
    pub long_term_db_path: Option<String>,
    pub enable_semantic_search: bool,
    pub embedding_model: String,
    pub context_window: usize,
    pub compaction_interval_secs: u64,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            short_term_capacity: 100,
            long_term_db_path: None,
            enable_semantic_search: true,
            embedding_model: "all-MiniLM-L6-v2".to_string(),
            context_window: 4096,
            compaction_interval_secs: 3600,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_memory_config() {
        let config = MemoryConfig::default();
        assert_eq!(config.short_term_capacity, 100);
        assert!(config.enable_semantic_search);
    }
}
```

- [ ] **Step 6: Create skills configuration**

Create `crates/kod-config/src/skills.rs`:

```rust
//! Skills system configuration.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SkillsConfig {
    pub skills_dir: Option<String>,
    pub enable_hot_reload: bool,
    pub max_cache_size_mb: usize,
    pub max_skills_per_query: usize,
    pub match_threshold: f32,
}

impl Default for SkillsConfig {
    fn default() -> Self {
        Self {
            skills_dir: None,
            enable_hot_reload: true,
            max_cache_size_mb: 50,
            max_skills_per_query: 3,
            match_threshold: 0.7,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_skills_config() {
        let config = SkillsConfig::default();
        assert!(config.enable_hot_reload);
        assert_eq!(config.max_skills_per_query, 3);
    }
}
```

- [ ] **Step 7: Run all kod-config tests**

```bash
cargo test -p kod-config
```

Expected: All tests pass

- [ ] **Step 8: Commit kod-config**

```bash
git add crates/kod-config/
git commit -m "feat(kod-config): add configuration system with TOML serialization"
```

---

### Task 5: Create Stub Crates for Remaining Workspace Members

**Files:**
- Create: `crates/kod-provider/Cargo.toml`
- Create: `crates/kod-provider/src/lib.rs`
- Create: `crates/kod-provider-ollama/Cargo.toml`
- Create: `crates/kod-provider-ollama/src/lib.rs`
- Create: `crates/kod-skills/Cargo.toml`
- Create: `crates/kod-skills/src/lib.rs`
- Create: `crates/kod-memory/Cargo.toml`
- Create: `crates/kod-memory/src/lib.rs`
- Create: `crates/kod-tools/Cargo.toml`
- Create: `crates/kod-tools/src/lib.rs`
- Create: `crates/kod-swarm/Cargo.toml`
- Create: `crates/kod-swarm/src/lib.rs`
- Create: `crates/kod-tui/Cargo.toml`
- Create: `crates/kod-tui/src/lib.rs`
- Create: `crates/kod-cli/Cargo.toml`
- Create: `crates/kod-cli/src/lib.rs`
- Create: `crates/kod-core/Cargo.toml`
- Create: `crates/kod-core/src/lib.rs`

- [ ] **Step 1: Create kod-provider stub**

Create `crates/kod-provider/Cargo.toml`:

```toml
[package]
name = "kod-provider"
version.workspace = true
edition.workspace = true
license.workspace = true
authors.workspace = true

[dependencies]
async-trait = "0.1"
serde = { workspace = true }
serde_json = { workspace = true }
tokio = { workspace = true }
futures = { workspace = true }
kod-types = { path = "../kod-types" }
kod-error = { path = "../kod-error" }
```

Create `crates/kod-provider/src/lib.rs`:

```rust
//! LLM provider abstraction layer.
//! 
//! This crate defines the traits and types that all LLM providers must implement.

pub mod traits;
pub mod types;

pub use traits::LlmProvider;
pub use types::*;
```

Create `crates/kod-provider/src/traits.rs`:

```rust
//! Provider traits for LLM integration.

use async_trait::async_trait;
use kod_error::Result;
use kod_types::{ToolCall, ToolDefinition};
use std::pin::Pin;
use futures::Stream;

#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// Get the provider name
    fn name(&self) -> &str;

    /// List available models
    async fn list_models(&self) -> Result<Vec<String>>;

    /// Generate a completion without tools
    async fn generate(&self, prompt: &str, options: &GenerationOptions) -> Result<String>;

    /// Generate a completion with tool calling support
    async fn generate_with_tools(
        &self,
        prompt: &str,
        tools: &[ToolDefinition],
        options: &GenerationOptions,
    ) -> Result<GenerationResponse>;

    /// Generate a streaming completion
    fn stream(
        &self,
        prompt: &str,
        options: &GenerationOptions,
    ) -> Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send + '_>>;
}

#[derive(Debug, Clone, Default)]
pub struct GenerationOptions {
    pub model: Option<String>,
    pub max_tokens: Option<usize>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub stop_sequences: Vec<String>,
}
```

Create `crates/kod-provider/src/types.rs`:

```rust
//! Types for LLM generation.

use kod_types::ToolCall;

#[derive(Debug, Clone)]
pub enum GenerationResponse {
    Text { content: String },
    ToolCalls { calls: Vec<ToolCall> },
    Mixed { content: String, calls: Vec<ToolCall> },
}

#[derive(Debug, Clone)]
pub enum StreamChunk {
    Text(String),
    ToolCallStart { name: String },
    ToolCallDelta { arguments: String },
    Done,
}
```

- [ ] **Step 2: Create kod-provider-ollama stub**

Create `crates/kod-provider-ollama/Cargo.toml`:

```toml
[package]
name = "kod-provider-ollama"
version.workspace = true
edition.workspace = true
license.workspace = true
authors.workspace = true

[dependencies]
async-trait = "0.1"
serde = { workspace = true }
serde_json = { workspace = true }
tokio = { workspace = true }
reqwest = { workspace = true }
futures = { workspace = true }
eventsource-stream = { workspace = true }
kod-types = { path = "../kod-types" }
kod-error = { path = "../kod-error" }
kod-provider = { path = "../kod-provider" }
```

Create `crates/kod-provider-ollama/src/lib.rs`:

```rust
//! Ollama LLM provider implementation.

pub mod client;
pub mod generate;

pub use client::OllamaClient;
```

- [ ] **Step 3: Create remaining stub crates**

For each remaining crate (kod-skills, kod-memory, kod-tools, kod-swarm, kod-tui, kod-cli, kod-core), create a minimal `Cargo.toml` and `lib.rs` with a placeholder comment.

Example for `crates/kod-skills/Cargo.toml`:

```toml
[package]
name = "kod-skills"
version.workspace = true
edition.workspace = true
license.workspace = true
authors.workspace = true

[dependencies]
serde = { workspace = true }
serde_json = { workspace = true }
walkdir = { workspace = true }
notify = { workspace = true }
tokio = { workspace = true }
kod-types = { path = "../kod-types" }
kod-error = { path = "../kod-error" }
```

And `crates/kod-skills/src/lib.rs`:

```rust
//! Skills system for markdown-based skill management.
//! 
//! This crate handles loading, parsing, matching, and hot-reloading
//! of skill files from ~/.kod/skills/

// TODO: Implement in Chunk 5
```

- [ ] **Step 4: Verify workspace builds**

```bash
cargo build --workspace
```

Expected: Workspace compiles successfully with warnings about unused code

- [ ] **Step 5: Run workspace tests**

```bash
cargo test --workspace
```

Expected: All tests pass

- [ ] **Step 6: Commit workspace stubs**

```bash
git add crates/
git commit -m "feat: add workspace member stubs for all crates"
```

---

### Task 6: Add Development Tooling

**Files:**
- Create: `Makefile`
- Create: `deny.toml`
- Create: `clippy.toml`

- [ ] **Step 1: Create Makefile**

```makefile
.PHONY: build test lint format check clean bench doc

build:
	cargo build --workspace

test:
	cargo test --workspace

lint:
	cargo clippy --workspace --all-targets -- -D warnings

format:
	cargo fmt --all

check: format lint test

clean:
	cargo clean

bench:
	cargo bench --workspace

doc:
	cargo doc --workspace --no-deps --open

# Development helpers
dev: build
	cargo run -- kod chat

install:
	cargo install --path crates/kod-cli
```

- [ ] **Step 2: Create clippy.toml**

```toml
# Clippy configuration
too-many-arguments-threshold = 8
type-complexity-threshold = 300
```

- [ ] **Step 3: Run clippy and fix any issues**

```bash
cargo clippy --workspace --all-targets -- -D warnings
```

Expected: No warnings

- [ ] **Step 4: Commit tooling**

```bash
git add Makefile clippy.toml
git commit -m "chore: add development tooling (Makefile, clippy config)"
```

---

## Chunk 1 Review Checklist

- [ ] Workspace builds successfully
- [ ] All tests pass
- [ ] Clippy passes with no warnings
- [ ] Type-safe IDs prevent mixing different ID types
- [ ] Error types cover all error cases from spec
- [ ] Configuration can be loaded/saved/serialized
- [ ] Stub crates are in place for future implementation

**Next Steps:** In the next message, I'll provide Chunk 2 covering the LLM Provider implementations (Ollama integration with streaming and tool calling).

---

Would you like me to continue with Chunk 2 (LLM Provider Implementation)?
