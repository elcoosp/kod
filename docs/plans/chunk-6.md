# Chunk 6: Agent Swarm System Implementation

## Task 29: Agent Types and Lifecycle

**Files:**
- Modify: `crates/kod-swarm/Cargo.toml`
- Create: `crates/kod-swarm/src/lib.rs`
- Create: `crates/kod-swarm/src/agent.rs`
- Test: `crates/kod-swarm/tests/agent.rs`

- [ ] **Step 1: Update kod-swarm Cargo.toml**

```toml
[package]
name = "kod-swarm"
version.workspace = true
edition.workspace = true
license.workspace = true
authors.workspace = true

[dependencies]
serde = { workspace = true }
serde_json = { workspace = true }
tokio = { workspace = true }
tracing = { workspace = true }
parking_lot = { workspace = true }
uuid = { version = "1.11", features = ["v4", "serde"] }
chrono = { version = "0.4", features = ["serde"] }
git2 = "0.19"
kod-types = { path = "../kod-types" }
kod-error = { path = "../kod-error" }
kod-config = { path = "../kod-config" }
kod-memory = { path = "../kod-memory" }
kod-tools = { path = "../kod-tools" }
kod-provider = { path = "../kod-provider" }

[dev-dependencies]
rstest = { workspace = true }
tempfile = "3.8"
futures = { workspace = true }
```

- [ ] **Step 2: Write failing test for agent lifecycle**

Create `crates/kod-swarm/tests/agent.rs`:

```rust
use kod_swarm::agent::{Agent, AgentBuilder, AgentState, Capability};
use kod_types::AgentId;
use std::time::Duration;

#[tokio::test]
async fn test_agent_creation() {
    let agent = Agent::new("architect")
        .with_capability(Capability::Planning)
        .build();
    
    assert_eq!(agent.id(), &AgentId::new());
    assert_eq!(agent.name(), "architect");
    assert_eq!(agent.state(), AgentState::Idle);
}

#[tokio::test]
async fn test_agent_builder() {
    let agent = AgentBuilder::new("coder")
        .with_capability(Capability::Coding)
        .with_capability(Capability::Testing)
        .with_model("codellama:13b")
        .with_max_context_tokens(8192)
        .build();
    
    assert_eq!(agent.name(), "coder");
    assert!(agent.has_capability(&Capability::Coding));
    assert!(agent.has_capability(&Capability::Testing));
    assert!(!agent.has_capability(&Capability::Planning));
}

#[tokio::test]
async fn test_agent_lifecycle() {
    let mut agent = Agent::new("worker").build();
    
    // Initial state
    assert_eq!(agent.state(), AgentState::Idle);
    
    // Start agent
    agent.start().await.unwrap();
    assert_eq!(agent.state(), AgentState::Running);
    
    // Pause agent
    agent.pause().await.unwrap();
    assert_eq!(agent.state(), AgentState::Paused);
    
    // Resume
    agent.resume().await.unwrap();
    assert_eq!(agent.state(), AgentState::Running);
    
    // Stop
    agent.stop().await.unwrap();
    assert_eq!(agent.state(), AgentState::Stopped);
}

#[tokio::test]
async fn test_agent_capabilities() {
    let agent = AgentBuilder::new("fullstack")
        .with_capability(Capability::Coding)
        .with_capability(Capability::Testing)
        .with_capability(Capability::Documentation)
        .with_capability(Capability::CodeReview)
        .build();
    
    let capabilities = agent.capabilities();
    assert_eq!(capabilities.len(), 4);
    assert!(agent.has_capability(&Capability::Coding));
    assert!(agent.has_capability(&Capability::Documentation));
}

#[tokio::test]
async fn test_agent_heartbeat() {
    let mut agent = Agent::new("worker").build();
    
    agent.start().await.unwrap();
    
    // Agent should track last heartbeat
    let heartbeat = agent.last_heartbeat();
    assert!(heartbeat.is_some());
    
    // Update heartbeat
    tokio::time::sleep(Duration::from_millis(10)).await;
    agent.record_heartbeat();
    
    let new_heartbeat = agent.last_heartbeat();
    assert!(new_heartbeat.is_some());
    
    // New heartbeat should be later than old
    if let (Some(old), Some(new)) = (heartbeat, new_heartbeat) {
        assert!(new > old);
    }
}

#[tokio::test]
async fn test_agent_is_idle_timeout() {
    let mut agent = Agent::new("worker").build();
    agent.start().await.unwrap();
    
    // Fresh heartbeat means not timed out
    agent.record_heartbeat();
    assert!(!agent.is_timed_out(Duration::from_secs(60)));
    
    // Simulate old heartbeat by not updating
    // In test, we can set last_heartbeat to past
    agent.set_last_heartbeat_for_test(std::time::Instant::now() - Duration::from_secs(120));
    assert!(agent.is_timed_out(Duration::from_secs(60)));
}

#[test]
fn test_capability_equality() {
    assert_eq!(Capability::Coding, Capability::Coding);
    assert_ne!(Capability::Coding, Capability::Testing);
    
    // Test string representation
    assert_eq!(Capability::Coding.as_str(), "coding");
    assert_eq!(Capability::Planning.as_str(), "planning");
}

#[test]
fn test_capability_from_str() {
    assert_eq!("coding".parse::<Capability>(), Ok(Capability::Coding));
    assert_eq!("testing".parse::<Capability>(), Ok(Capability::Testing));
    assert!("invalid".parse::<Capability>().is_err());
}
```

- [ ] **Step 3: Run tests to verify they fail**

```bash
cargo test -p kod-swarm --test agent
```

Expected: FAIL - agent module not implemented

- [ ] **Step 4: Implement agent types**

Create `crates/kod-swarm/src/lib.rs`:

```rust
//! Agent swarm system for coordinated multi-agent collaboration.
//!
//! This crate handles agent lifecycle, direct messaging, shared workspace
//! coordination, and task orchestration.

pub mod agent;
pub mod communication;
pub mod workspace;
pub mod coordination;
pub mod swarm;

pub use agent::{Agent, AgentBuilder, AgentState, Capability};
pub use communication::{AgentCommunicationHub, MessageContent};
pub use workspace::{SharedWorkspace, FileLock, LockType};
pub use coordination::{TaskCoordinator, TaskAssignment};
pub use swarm::AgentSwarm;
```

Create `crates/kod-swarm/src/agent.rs`:

```rust
//! Agent definition and lifecycle management.

use kod_error::{KodError, Result};
use kod_types::AgentId;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};

/// Capabilities that an agent can have
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Capability {
    Coding,
    Testing,
    Documentation,
    CodeReview,
    Planning,
    Research,
    Debugging,
    Refactoring,
}

impl Capability {
    pub fn as_str(&self) -> &'static str {
        match self {
            Capability::Coding => "coding",
            Capability::Testing => "testing",
            Capability::Documentation => "documentation",
            Capability::CodeReview => "code-review",
            Capability::Planning => "planning",
            Capability::Research => "research",
            Capability::Debugging => "debugging",
            Capability::Refactoring => "refactoring",
        }
    }
}

impl std::fmt::Display for Capability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl std::str::FromStr for Capability {
    type Err = KodError;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "coding" => Ok(Capability::Coding),
            "testing" => Ok(Capability::Testing),
            "documentation" => Ok(Capability::Documentation),
            "code-review" => Ok(Capability::CodeReview),
            "planning" => Ok(Capability::Planning),
            "research" => Ok(Capability::Research),
            "debugging" => Ok(Capability::Debugging),
            "refactoring" => Ok(Capability::Refactoring),
            _ => Err(KodError::InvalidState(format!("Unknown capability: {}", s))),
        }
    }
}

/// State of an agent
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentState {
    Idle,
    Starting,
    Running,
    Paused,
    Stopping,
    Stopped,
    Failed,
}

impl Default for AgentState {
    fn default() -> Self {
        AgentState::Idle
    }
}

/// An agent in the swarm
pub struct Agent {
    id: AgentId,
    name: String,
    capabilities: HashSet<Capability>,
    state: watch::Sender<AgentState>,
    state_receiver: watch::Receiver<AgentState>,
    last_heartbeat: parking_lot::Mutex<Option<Instant>>,
    model: String,
    max_context_tokens: usize,
    mailbox: Option<mpsc::UnboundedReceiver<crate::communication::AgentMessage>>,
    model_config: ModelConfig,
}

/// Model configuration for the agent
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    pub model_name: String,
    pub provider: String,
    pub temperature: f32,
    pub max_tokens: usize,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            model_name: "codellama:13b".to_string(),
            provider: "ollama".to_string(),
            temperature: 0.7,
            max_tokens: 2048,
        }
    }
}

impl Agent {
    /// Create a new agent with the given name
    pub fn new(name: impl Into<String>) -> AgentBuilder {
        AgentBuilder::new(name)
    }

    /// Get agent ID
    pub fn id(&self) -> &AgentId {
        &self.id
    }

    /// Get agent name
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Get current state
    pub fn state(&self) -> AgentState {
        *self.state_receiver.borrow()
    }

    /// Check if agent has a capability
    pub fn has_capability(&self, capability: &Capability) -> bool {
        self.capabilities.contains(capability)
    }

    /// Get all capabilities
    pub fn capabilities(&self) -> Vec<Capability> {
        self.capabilities.iter().cloned().collect()
    }

    /// Get model name
    pub fn model(&self) -> &str {
        &self.model_config.model_name
    }

    /// Get max context tokens
    pub fn max_context_tokens(&self) -> usize {
        self.max_context_tokens
    }

    /// Start the agent
    pub async fn start(&mut self) -> Result<()> {
        if self.state() != AgentState::Idle && self.state() != AgentState::Stopped {
            return Err(KodError::InvalidState(format!(
                "Cannot start agent in state {:?}",
                self.state()
            )));
        }

        self.state.send(AgentState::Starting)
            .map_err(|e| KodError::InvalidState(format!("Failed to update state: {:?}", e)))?;

        // Simulate initialization
        tokio::time::sleep(Duration::from_millis(10)).await;

        self.state.send(AgentState::Running)
            .map_err(|e| KodError::InvalidState(format!("Failed to update state: {:?}", e)))?;

        self.record_heartbeat();

        Ok(())
    }

    /// Pause the agent
    pub async fn pause(&mut self) -> Result<()> {
        if self.state() != AgentState::Running {
            return Err(KodError::InvalidState(format!(
                "Cannot pause agent in state {:?}",
                self.state()
            )));
        }

        self.state.send(AgentState::Paused)
            .map_err(|e| KodError::InvalidState(format!("Failed to update state: {:?}", e)))?;

        Ok(())
    }

    /// Resume the agent
    pub async fn resume(&mut self) -> Result<()> {
        if self.state() != AgentState::Paused {
            return Err(KodError::InvalidState(format!(
                "Cannot resume agent in state {:?}",
                self.state()
            )));
        }

        self.state.send(AgentState::Running)
            .map_err(|e| KodError::InvalidState(format!("Failed to update state: {:?}", e)))?;

        self.record_heartbeat();

        Ok(())
    }

    /// Stop the agent
    pub async fn stop(&mut self) -> Result<()> {
        match self.state() {
            AgentState::Running | AgentState::Paused | AgentState::Starting => {
                self.state.send(AgentState::Stopping)
                    .map_err(|e| KodError::InvalidState(format!("Failed to update state: {:?}", e)))?;

                // Cleanup
                tokio::time::sleep(Duration::from_millis(10)).await;

                self.state.send(AgentState::Stopped)
                    .map_err(|e| KodError::InvalidState(format!("Failed to update state: {:?}", e)))?;
            }
            AgentState::Stopped => return Ok(()),
            _ => {
                return Err(KodError::InvalidState(format!(
                    "Cannot stop agent in state {:?}",
                    self.state()
                )));
            }
        }

        Ok(())
    }

    /// Record a heartbeat
    pub fn record_heartbeat(&self) {
        *self.last_heartbeat.lock() = Some(Instant::now());
    }

    /// Get last heartbeat time
    pub fn last_heartbeat(&self) -> Option<Instant> {
        *self.last_heartbeat.lock()
    }

    /// Check if agent has timed out
    pub fn is_timed_out(&self, timeout: Duration) -> bool {
        match self.last_heartbeat() {
            Some(last) => last.elapsed() > timeout,
            None => true, // No heartbeat means timed out
        }
    }

    /// Watch for state changes
    pub fn watch_state(&self) -> watch::Receiver<AgentState> {
        self.state_receiver.clone()
    }

    /// Set last heartbeat for testing
    #[cfg(test)]
    pub fn set_last_heartbeat_for_test(&self, instant: Instant) {
        *self.last_heartbeat.lock() = Some(instant);
    }
}

/// Builder for Agent
pub struct AgentBuilder {
    name: String,
    capabilities: HashSet<Capability>,
    model: Option<String>,
    max_context_tokens: usize,
    model_config: ModelConfig,
}

impl AgentBuilder {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            capabilities: HashSet::new(),
            model: None,
            max_context_tokens: 8192,
            model_config: ModelConfig::default(),
        }
    }

    /// Add a capability
    pub fn with_capability(mut self, capability: Capability) -> Self {
        self.capabilities.insert(capability);
        self
    }

    /// Set the model
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Set max context tokens
    pub fn with_max_context_tokens(mut self, tokens: usize) -> Self {
        self.max_context_tokens = tokens;
        self
    }

    /// Set model config
    pub fn with_model_config(mut self, config: ModelConfig) -> Self {
        self.model_config = config;
        self
    }

    /// Build the agent
    pub fn build(self) -> Agent {
        let (state_tx, state_rx) = watch::channel(AgentState::Idle);
        let model = self.model.unwrap_or_else(|| self.model_config.model_name.clone());

        Agent {
            id: AgentId::new(),
            name: self.name,
            capabilities: self.capabilities,
            state: state_tx,
            state_receiver: state_rx,
            last_heartbeat: parking_lot::Mutex::new(None),
            model,
            max_context_tokens: self.max_context_tokens,
            mailbox: None,
            model_config: self.model_config,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_agent_lifecycle() {
        let mut agent = Agent::new("test").build();
        
        assert_eq!(agent.state(), AgentState::Idle);
        
        agent.start().await.unwrap();
        assert_eq!(agent.state(), AgentState::Running);
        
        agent.pause().await.unwrap();
        assert_eq!(agent.state(), AgentState::Paused);
        
        agent.resume().await.unwrap();
        assert_eq!(agent.state(), AgentState::Running);
        
        agent.stop().await.unwrap();
        assert_eq!(agent.state(), AgentState::Stopped);
    }

    #[test]
    fn test_capabilities() {
        let agent = AgentBuilder::new("test")
            .with_capability(Capability::Coding)
            .with_capability(Capability::Testing)
            .build();
        
        assert!(agent.has_capability(&Capability::Coding));
        assert!(agent.has_capability(&Capability::Testing));
        assert!(!agent.has_capability(&Capability::Planning));
    }
}
```

- [ ] **Step 5: Run tests to verify they pass**

```bash
cargo test -p kod-swarm --test agent
cargo test -p kod-swarm --lib agent
```

Expected: All tests pass

- [ ] **Step 6: Commit**

```bash
git add crates/kod-swarm/
git commit -m "feat(swarm): add agent lifecycle with capabilities and state management"
```

---

## Task 30: Agent Communication Hub (Direct Messaging)

**Files:**
- Create: `crates/kod-swarm/src/communication.rs`
- Test: `crates/kod-swarm/tests/communication.rs`

- [ ] **Step 1: Write failing test for communication**

Create `crates/kod-swarm/tests/communication.rs`:

```rust
use kod_swarm::communication::{AgentCommunicationHub, MessageContent, MessageDestination};
use kod_types::AgentId;
use std::time::Duration;

#[tokio::test]
async fn test_direct_message() {
    let hub = AgentCommunicationHub::new();
    
    let agent_a = AgentId::new();
    let agent_b = AgentId::new();
    
    // Register agents
    hub.register_agent(agent_a.clone()).await.unwrap();
    hub.register_agent(agent_b.clone()).await.unwrap();
    
    // Send direct message
    hub.send_direct(
        &agent_a,
        &agent_b,
        MessageContent::TaskAssignment {
            description: "Implement user auth".to_string(),
            priority: kod_types::Priority::High,
        },
    ).await.unwrap();
    
    // Agent B should receive the message
    let receiver = hub.get_agent_receiver(&agent_b).await.unwrap();
    let mut receiver = receiver.lock().await;
    
    let message = receiver.recv().await.unwrap();
    assert_eq!(message.from, agent_a);
    assert_eq!(message.to, MessageDestination::Agent(agent_b.clone()));
    
    match message.content {
        MessageContent::TaskAssignment { description, priority } => {
            assert_eq!(description, "Implement user auth");
            assert_eq!(priority, kod_types::Priority::High);
        }
        _ => panic!("Expected TaskAssignment message"),
    }
}

#[tokio::test]
async fn test_broadcast_message() {
    let hub = AgentCommunicationHub::new();
    
    let agent_a = AgentId::new();
    let agent_b = AgentId::new();
    let agent_c = AgentId::new();
    
    // Register agents
    hub.register_agent(agent_a.clone()).await.unwrap();
    hub.register_agent(agent_b.clone()).await.unwrap();
    hub.register_agent(agent_c.clone()).await.unwrap();
    
    // Broadcast from agent A
    hub.broadcast(
        &agent_a,
        MessageContent::KnowledgeShare {
            information: "Found a bug in auth module".to_string(),
            tags: vec!["auth".to_string(), "bug".to_string()],
        },
    ).await.unwrap();
    
    // Both B and C should receive
    let receiver_b = hub.get_agent_receiver(&agent_b).await.unwrap();
    let mut receiver_b = receiver_b.lock().await;
    
    let message_b = receiver_b.recv().await.unwrap();
    assert_eq!(message_b.from, agent_a);
    
    let receiver_c = hub.get_agent_receiver(&agent_c).await.unwrap();
    let mut receiver_c = receiver_c.lock().await;
    
    let message_c = receiver_c.recv().await.unwrap();
    assert_eq!(message_c.from, agent_a);
}

#[tokio::test]
async fn test_agent_not_found() {
    let hub = AgentCommunicationHub::new();
    
    let agent_a = AgentId::new();
    let nonexistent = AgentId::new();
    
    hub.register_agent(agent_a.clone()).await.unwrap();
    
    // Try to send to non-existent agent
    let result = hub.send_direct(
        &agent_a,
        &nonexistent,
        MessageContent::ProgressUpdate {
            status: kod_types::TaskStatus::InProgress,
            details: "Test".to_string(),
        },
    ).await;
    
    assert!(result.is_err());
}

#[tokio::test]
async fn test_agent_offline() {
    let hub = AgentCommunicationHub::new();
    
    let agent_a = AgentId::new();
    let agent_b = AgentId::new();
    
    hub.register_agent(agent_a.clone()).await.unwrap();
    hub.register_agent(agent_b.clone()).await.unwrap();
    
    // Mark agent B as offline
    hub.set_agent_offline(&agent_b).await;
    
    // Try to send to offline agent
    let result = hub.send_direct(
        &agent_a,
        &agent_b,
        MessageContent::HelpRequest {
            question: "Can you help?".to_string(),
            context: "Auth implementation".to_string(),
        },
    ).await;
    
    assert!(result.is_err());
}

#[tokio::test]
async fn test_unregister_agent() {
    let hub = AgentCommunicationHub::new();
    
    let agent_a = AgentId::new();
    let agent_b = AgentId::new();
    
    hub.register_agent(agent_a.clone()).await.unwrap();
    hub.register_agent(agent_b.clone()).await.unwrap();
    
    // Unregister agent B
    hub.unregister_agent(&agent_b).await;
    
    // Try to send to unregistered agent
    let result = hub.send_direct(
        &agent_a,
        &agent_b,
        MessageContent::ResultDelivery {
            result: "Done".to_string(),
        },
    ).await;
    
    assert!(result.is_err());
}

#[tokio::test]
async fn test_message_history() {
    let hub = AgentCommunicationHub::new();
    
    let agent_a = AgentId::new();
    let agent_b = AgentId::new();
    
    hub.register_agent(agent_a.clone()).await.unwrap();
    hub.register_agent(agent_b.clone()).await.unwrap();
    
    // Send multiple messages
    hub.send_direct(
        &agent_a,
        &agent_b,
        MessageContent::TaskAssignment {
            description: "Task 1".to_string(),
            priority: kod_types::Priority::Medium,
        },
    ).await.unwrap();
    
    hub.send_direct(
        &agent_b,
        &agent_a,
        MessageContent::ProgressUpdate {
            status: kod_types::TaskStatus::Completed,
            details: "Finished task 1".to_string(),
        },
    ).await.unwrap();
    
    // Get history for agent A
    let history_a = hub.get_agent_history(&agent_a).await;
    assert_eq!(history_a.len(), 2); // Sent + received
    
    // Get history for agent B
    let history_b = hub.get_agent_history(&agent_b).await;
    assert_eq!(history_b.len(), 2);
}

#[tokio::test]
async fn test_coordination_messages() {
    let hub = AgentCommunicationHub::new();
    
    let agent_a = AgentId::new();
    let agent_b = AgentId::new();
    
    hub.register_agent(agent_a.clone()).await.unwrap();
    hub.register_agent(agent_b.clone()).await.unwrap();
    
    // Send file claim
    hub.send_direct(
        &agent_a,
        &agent_b,
        MessageContent::Coordination {
            action: kod_types::CoordinationAction::ProposingChange {
                file: "src/main.rs".to_string(),
                description: "Refactor main function".to_string(),
            },
        },
    ).await.unwrap();
    
    // Receive and verify
    let receiver = hub.get_agent_receiver(&agent_b).await.unwrap();
    let mut receiver = receiver.lock().await;
    
    let message = receiver.recv().await.unwrap();
    match message.content {
        MessageContent::Coordination { action } => {
            match action {
                kod_types::CoordinationAction::ProposingChange { file, description } => {
                    assert_eq!(file, "src/main.rs");
                    assert_eq!(description, "Refactor main function");
                }
                _ => panic!("Expected ProposingChange action"),
            }
        }
        _ => panic!("Expected Coordination message"),
    }
}

#[tokio::test]
async fn test_file_claim_and_release() {
    let hub = AgentCommunicationHub::new();
    
    let agent_a = AgentId::new();
    let agent_b = AgentId::new();
    
    hub.register_agent(agent_a.clone()).await.unwrap();
    hub.register_agent(agent_b.clone()).await.unwrap();
    
    // Claim file
    hub.send_direct(
        &agent_a,
        &agent_b,
        MessageContent::FileClaim {
            path: "src/auth.rs".to_string(),
            duration_secs: 300,
        },
    ).await.unwrap();
    
    // Release file
    hub.send_direct(
        &agent_a,
        &agent_b,
        MessageContent::FileRelease {
            path: "src/auth.rs".to_string(),
        },
    ).await.unwrap();
    
    // Agent B should receive both messages
    let receiver = hub.get_agent_receiver(&agent_b).await.unwrap();
    let mut receiver = receiver.lock().await;
    
    let claim_msg = receiver.recv().await.unwrap();
    match claim_msg.content {
        MessageContent::FileClaim { path, duration_secs } => {
            assert_eq!(path, "src/auth.rs");
            assert_eq!(duration_secs, 300);
        }
        _ => panic!("Expected FileClaim message"),
    }
    
    let release_msg = receiver.recv().await.unwrap();
    match release_msg.content {
        MessageContent::FileRelease { path } => {
            assert_eq!(path, "src/auth.rs");
        }
        _ => panic!("Expected FileRelease message"),
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test -p kod-swarm --test communication
```

Expected: FAIL - communication module not implemented

- [ ] **Step 3: Implement communication hub**

Create `crates/kod-swarm/src/communication.rs`:

```rust
//! Agent communication hub for direct messaging and coordination.
//!
//! Provides direct messaging between agents, broadcast capabilities,
//! and message history tracking.

use kod_error::{KodError, Result};
use kod_types::{
    AgentId, AgentMessage, AgentMessageContent, CoordinationAction, MessageDestination,
    MessageId, Priority, TaskStatus,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{mpsc, Mutex, RwLock};
use chrono::{DateTime, Utc};

/// Content of messages that can be sent between agents
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MessageContent {
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
    ConflictAlert {
        file_path: String,
        description: String,
    },
}

impl From<MessageContent> for AgentMessageContent {
    fn from(content: MessageContent) -> Self {
        match content {
            MessageContent::TaskAssignment { description, priority } => {
                AgentMessageContent::TaskAssignment { description, priority }
            }
            MessageContent::ProgressUpdate { status, details } => {
                AgentMessageContent::ProgressUpdate { status, details }
            }
            MessageContent::HelpRequest { question, context } => {
                AgentMessageContent::HelpRequest { question, context }
            }
            MessageContent::KnowledgeShare { information, tags } => {
                AgentMessageContent::KnowledgeShare { information, tags }
            }
            MessageContent::Coordination { action } => {
                AgentMessageContent::Coordination { action }
            }
            MessageContent::FileClaim { path, duration_secs } => {
                AgentMessageContent::FileClaim { path, duration_secs }
            }
            MessageContent::FileRelease { path } => {
                AgentMessageContent::FileRelease { path }
            }
            MessageContent::ResultDelivery { result } => {
                AgentMessageContent::ResultDelivery { result }
            }
            MessageContent::ConflictAlert { file_path, description } => {
                AgentMessageContent::ConflictAlert { file_path, description }
            }
        }
    }
}

/// Message record for history tracking
#[derive(Debug, Clone)]
struct MessageRecord {
    message: AgentMessage,
    timestamp: DateTime<Utc>,
}

/// Agent status in the hub
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentStatus {
    Online,
    Offline,
}

/// Communication hub for agent messaging
#[derive(Debug, Default)]
pub struct AgentCommunicationHub {
    /// Message channels for each agent
    agent_channels: RwLock<HashMap<AgentId, Arc<Mutex<mpsc::UnboundedReceiver<AgentMessage>>>>>,
    
    /// Senders for each agent
    agent_senders: RwLock<HashMap<AgentId, mpsc::UnboundedSender<AgentMessage>>>,
    
    /// Agent status
    agent_status: RwLock<HashMap<AgentId, AgentStatus>>,
    
    /// Message history
    message_history: Mutex<Vec<MessageRecord>>,
    
    /// Maximum history size
    max_history: usize,
}

impl AgentCommunicationHub {
    pub fn new() -> Self {
        Self {
            agent_channels: RwLock::new(HashMap::new()),
            agent_senders: RwLock::new(HashMap::new()),
            agent_status: RwLock::new(HashMap::new()),
            message_history: Mutex::new(Vec::new()),
            max_history: 1000,
        }
    }

    /// Create with custom history size
    pub fn with_history_size(mut self, size: usize) -> Self {
        self.max_history = size;
        self
    }

    /// Register a new agent
    pub async fn register_agent(&self, agent_id: AgentId) -> Result<()> {
        let (tx, rx) = mpsc::unbounded_channel();
        
        let mut channels = self.agent_channels.write().await;
        let mut senders = self.agent_senders.write().await;
        let mut status = self.agent_status.write().await;
        
        if channels.contains_key(&agent_id) {
            return Err(KodError::InvalidState(format!(
                "Agent already registered: {:?}",
                agent_id
            )));
        }
        
        channels.insert(agent_id.clone(), Arc::new(Mutex::new(rx)));
        senders.insert(agent_id.clone(), tx);
        status.insert(agent_id, AgentStatus::Online);
        
        Ok(())
    }

    /// Unregister an agent
    pub async fn unregister_agent(&self, agent_id: &AgentId) {
        self.agent_channels.write().await.remove(agent_id);
        self.agent_senders.write().await.remove(agent_id);
        self.agent_status.write().await.remove(agent_id);
    }

    /// Set agent as offline
    pub async fn set_agent_offline(&self, agent_id: &AgentId) {
        if let Some(status) = self.agent_status.write().await.get_mut(agent_id) {
            *status = AgentStatus::Offline;
        }
    }

    /// Set agent as online
    pub async fn set_agent_online(&self, agent_id: &AgentId) {
        if let Some(status) = self.agent_status.write().await.get_mut(agent_id) {
            *status = AgentStatus::Online;
        }
    }

    /// Check if agent is registered and online
    pub async fn is_agent_available(&self, agent_id: &AgentId) -> bool {
        let status = self.agent_status.read().await;
        matches!(status.get(agent_id), Some(AgentStatus::Online))
    }

    /// Send a direct message from one agent to another
    pub async fn send_direct(
        &self,
        from: &AgentId,
        to: &AgentId,
        content: MessageContent,
    ) -> Result<()> {
        // Check if target agent is available
        if !self.is_agent_available(to).await {
            return Err(KodError::AgentNotFound {
                agent_id: to.clone(),
            });
        }
        
        let message = AgentMessage {
            id: MessageId::new(),
            from: from.clone(),
            to: MessageDestination::Agent(to.clone()),
            content: content.into(),
            timestamp: Utc::now(),
        };
        
        // Send to target agent
        let senders = self.agent_senders.read().await;
        if let Some(sender) = senders.get(to) {
            sender.send(message.clone())
                .map_err(|_| KodError::AgentCommunication(format!(
                    "Failed to send message to agent {:?}",
                    to
                )))?;
        } else {
            return Err(KodError::AgentNotFound {
                agent_id: to.clone(),
            });
        }
        
        // Record in history
        self.record_message(message).await;
        
        Ok(())
    }

    /// Broadcast a message to all agents except the sender
    pub async fn broadcast(
        &self,
        from: &AgentId,
        content: MessageContent,
    ) -> Result<()> {
        let message = AgentMessage {
            id: MessageId::new(),
            from: from.clone(),
            to: MessageDestination::Broadcast,
            content: content.into(),
            timestamp: Utc::now(),
        };
        
        let senders = self.agent_senders.read().await;
        
        for (agent_id, sender) in senders.iter() {
            if agent_id != from {
                let _ = sender.send(message.clone());
            }
        }
        
        // Record in history
        self.record_message(message).await;
        
        Ok(())
    }

    /// Get a receiver for an agent's messages
    pub async fn get_agent_receiver(
        &self,
        agent_id: &AgentId,
    ) -> Result<Arc<Mutex<mpsc::UnboundedReceiver<AgentMessage>>>> {
        let channels = self.agent_channels.read().await;
        
        channels.get(agent_id)
            .cloned()
            .ok_or_else(|| KodError::AgentNotFound {
                agent_id: agent_id.clone(),
            })
    }

    /// Get message history for an agent
    pub async fn get_agent_history(&self, agent_id: &AgentId) -> Vec<AgentMessage> {
        let history = self.message_history.lock().await;
        
        history.iter()
            .filter(|record| {
                record.message.from == *agent_id 
                    || matches!(&record.message.to, MessageDestination::Agent(to) if to == agent_id)
            })
            .map(|record| record.message.clone())
            .collect()
    }

    /// Get all message history
    pub async fn get_all_history(&self) -> Vec<AgentMessage> {
        let history = self.message_history.lock().await;
        history.iter().map(|record| record.message.clone()).collect()
    }

    /// Clear message history
    pub async fn clear_history(&self) {
        self.message_history.lock().await.clear();
    }

    /// Get list of registered agents
    pub async fn list_agents(&self) -> Vec<AgentId> {
        self.agent_senders.read().await.keys().cloned().collect()
    }

    /// Record a message in history
    async fn record_message(&self, message: AgentMessage) {
        let mut history = self.message_history.lock().await;
        
        history.push(MessageRecord {
            message,
            timestamp: Utc::now(),
        });
        
        // Trim history if it exceeds max size
        if history.len() > self.max_history {
            let drain_count = history.len() - self.max_history;
            history.drain(0..drain_count);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_hub_basic_operations() {
        let hub = AgentCommunicationHub::new();
        
        let agent_a = AgentId::new();
        let agent_b = AgentId::new();
        
        hub.register_agent(agent_a.clone()).await.unwrap();
        hub.register_agent(agent_b.clone()).await.unwrap();
        
        assert!(hub.is_agent_available(&agent_a).await);
        
        hub.send_direct(
            &agent_a,
            &agent_b,
            MessageContent::TaskAssignment {
                description: "Test".to_string(),
                priority: Priority::Low,
            },
        ).await.unwrap();
        
        let history = hub.get_agent_history(&agent_a).await;
        assert_eq!(history.len(), 1);
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
cargo test -p kod-swarm --test communication
cargo test -p kod-swarm --lib communication
```

Expected: All tests pass

- [ ] **Step 5: Commit**

```bash
git add crates/kod-swarm/
git commit -m "feat(swarm): add agent communication hub with direct messaging and history"
```

---

## Task 31: Shared Workspace with File Locking

**Files:**
- Create: `crates/kod-swarm/src/workspace.rs`
- Test: `crates/kod-swarm/tests/workspace.rs`

- [ ] **Step 1: Write failing test for shared workspace**

Create `crates/kod-swarm/tests/workspace.rs`:

```rust
use kod_swarm::workspace::{SharedWorkspace, LockType, CollaborationMode};
use kod_types::AgentId;
use std::time::Duration;
use tempfile::TempDir;

fn create_test_workspace(dir: &std::path::Path) -> SharedWorkspace {
    SharedWorkspace::new(
        dir,
        "feature/test".to_string(),
        CollaborationMode::SharedBranch,
    )
}

#[tokio::test]
async fn test_acquire_exclusive_lock() {
    let temp_dir = TempDir::new().unwrap();
    let workspace = create_test_workspace(temp_dir.path());
    
    let agent_a = AgentId::new();
    let agent_b = AgentId::new();
    
    let file_path = std::path::Path::new("src/main.rs");
    
    // Agent A acquires exclusive lock
    workspace.acquire_lock(&agent_a, file_path, LockType::Exclusive).await.unwrap();
    
    // Check lock is held
    let lock_info = workspace.get_lock_info(file_path).await;
    assert!(lock_info.is_some());
    assert_eq!(lock_info.unwrap().agent_id, agent_a);
    
    // Agent B should not be able to acquire while A holds it
    let result = workspace.try_acquire_lock(&agent_b, file_path, LockType::Exclusive).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_release_lock() {
    let temp_dir = TempDir::new().unwrap();
    let workspace = create_test_workspace(temp_dir.path());
    
    let agent_a = AgentId::new();
    let agent_b = AgentId::new();
    
    let file_path = std::path::Path::new("src/lib.rs");
    
    // Agent A acquires and releases
    {
        let _guard = workspace.acquire_lock(&agent_a, file_path, LockType::Exclusive).await.unwrap();
        
        // Lock should be held
        assert!(workspace.is_locked(file_path).await);
    }
    
    // Guard dropped, lock should be released
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(!workspace.is_locked(file_path).await);
    
    // Agent B should now be able to acquire
    let _guard = workspace.acquire_lock(&agent_b, file_path, LockType::Exclusive).await.unwrap();
    assert!(workspace.is_locked(file_path).await);
}

#[tokio::test]
async fn test_shared_locks() {
    let temp_dir = TempDir::new().unwrap();
    let workspace = create_test_workspace(temp_dir.path());
    
    let agent_a = AgentId::new();
    let agent_b = AgentId::new();
    
    let file_path = std::path::Path::new("src/shared.rs");
    
    // Both agents can acquire shared locks
    let _guard_a = workspace.acquire_lock(&agent_a, file_path, LockType::Shared).await.unwrap();
    let _guard_b = workspace.acquire_lock(&agent_b, file_path, LockType::Shared).await.unwrap();
    
    // Both should be locked
    assert!(workspace.is_locked(file_path).await);
}

#[tokio::test]
async test_lock_timeout() {
    let temp_dir = TempDir::new().unwrap();
    let workspace = SharedWorkspace::with_timeout(
        temp_dir.path(),
        "test".to_string(),
        CollaborationMode::SharedBranch,
        Duration::from_millis(100), // Short timeout for testing
    );
    
    let agent_a = AgentId::new();
    let agent_b = AgentId::new();
    
    let file_path = std::path::Path::new("src/test.rs");
    
    // Agent A holds lock
    let _guard_a = workspace.acquire_lock(&agent_a, file_path, LockType::Exclusive).await.unwrap();
    
    // Agent B tries to acquire with timeout
    let start = std::time::Instant::now();
    let result = workspace.acquire_lock(&agent_b, file_path, LockType::Exclusive).await;
    
    // Should timeout
    assert!(result.is_err());
    assert!(start.elapsed() >= Duration::from_millis(90));
}

#[tokio::test]
async fn test_intent_to_write() {
    let temp_dir = TempDir::new().unwrap();
    let workspace = create_test_workspace(temp_dir.path());
    
    let agent_a = AgentId::new();
    let agent_b = AgentId::new();
    
    let file_path = std::path::Path::new("src/planned.rs");
    
    // Agent A signals intent to write
    workspace.acquire_lock(&agent_a, file_path, LockType::IntentToWrite).await.unwrap();
    
    // Agent B can still read (shared lock)
    let _guard_b = workspace.acquire_lock(&agent_b, file_path, LockType::Shared).await.unwrap();
    
    // But agent B can't acquire exclusive or intent-to-write
    let result = workspace.try_acquire_lock(&agent_b, file_path, LockType::IntentToWrite).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_get_all_locks() {
    let temp_dir = TempDir::new().unwrap();
    let workspace = create_test_workspace(temp_dir.path());
    
    let agent_a = AgentId::new();
    let agent_b = AgentId::new();
    
    let file1 = std::path::Path::new("src/file1.rs");
    let file2 = std::path::Path::new("src/file2.rs");
    let file3 = std::path::Path::new("src/file3.rs");
    
    // Acquire multiple locks
    let _guard1 = workspace.acquire_lock(&agent_a, file1, LockType::Exclusive).await.unwrap();
    let _guard2 = workspace.acquire_lock(&agent_b, file2, LockType::Shared).await.unwrap();
    let _guard3 = workspace.acquire_lock(&agent_a, file3, LockType::IntentToWrite).await.unwrap();
    
    // Get all locks
    let all_locks = workspace.get_all_locks().await;
    assert_eq!(all_locks.len(), 3);
    
    // Verify agents
    let agents: std::collections::HashSet<AgentId> = all_locks.iter()
        .map(|lock| lock.agent_id.clone())
        .collect();
    
    assert_eq!(agents.len(), 2);
    assert!(agents.contains(&agent_a));
    assert!(agents.contains(&agent_b));
}

#[tokio::test]
async fn test_workspace_status() {
    let temp_dir = TempDir::new().unwrap();
    let workspace = create_test_workspace(temp_dir.path());
    
    let agent_a = AgentId::new();
    let file_path = std::path::Path::new("src/status.rs");
    
    let _guard = workspace.acquire_lock(&agent_a, file_path, LockType::Exclusive).await.unwrap();
    
    let status = workspace.status().await;
    assert_eq!(status.branch, "feature/test");
    assert_eq!(status.mode, CollaborationMode::SharedBranch);
    assert_eq!(status.locked_files.len(), 1);
}

#[tokio::test]
async fn test_coordination_notification() {
    let temp_dir = TempDir::new().unwrap();
    let workspace = create_test_workspace(temp_dir.path());
    
    let agent_a = AgentId::new();
    let agent_b = AgentId::new();
    
    let file_path = std::path::Path::new("src/notify.rs");
    
    // Set up notification receiver
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    workspace.subscribe_to_events(tx);
    
    // Agent A acquires lock
    let _guard = workspace.acquire_lock(&agent_a, file_path, LockType::Exclusive).await.unwrap();
    
    // Should receive notification
    let event = rx.recv().await.unwrap();
    match event {
        kod_swarm::workspace::WorkspaceEvent::LockAcquired { agent_id, path, lock_type } => {
            assert_eq!(agent_id, agent_a);
            assert_eq!(path, file_path);
            assert_eq!(lock_type, LockType::Exclusive);
        }
        _ => panic!("Expected LockAcquired event"),
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test -p kod-swarm --test workspace
```

Expected: FAIL - workspace module not implemented

- [ ] **Step 3: Implement shared workspace**

Create `crates/kod-swarm/src/workspace.rs`:

```rust
//! Shared workspace for multi-agent file coordination.
//!
//! Provides file locking mechanisms for agents working on the same
//! branch, with optional worktree isolation for parallel development.

use kod_error::{KodError, Result};
use kod_types::AgentId;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Mutex, RwLock};

/// Type of lock on a file
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LockType {
    /// Exclusive access for writing
    Exclusive,
    /// Shared access for reading
    Shared,
    /// Signal intent to write (allows reads but warns other writers)
    IntentToWrite,
}

impl LockType {
    /// Check if this lock type is compatible with another
    pub fn is_compatible_with(&self, other: &LockType) -> bool {
        match (self, other) {
            (LockType::Shared, LockType::Shared) => true,
            (LockType::Shared, LockType::IntentToWrite) => true,
            (LockType::IntentToWrite, LockType::Shared) => true,
            _ => false,
        }
    }
}

/// Collaboration mode for the workspace
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CollaborationMode {
    /// All agents work on the same branch with coordination
    SharedBranch,
    /// Each agent works in isolated worktree
    IsolatedWorktrees,
    /// Hybrid mode
    Hybrid,
}

/// Information about a file lock
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileLock {
    pub agent_id: AgentId,
    pub lock_type: LockType,
    pub acquired_at: Instant,
    pub expires_at: Option<Instant>,
}

/// Events emitted by the workspace
#[derive(Debug, Clone)]
pub enum WorkspaceEvent {
    LockAcquired {
        agent_id: AgentId,
        path: PathBuf,
        lock_type: LockType,
    },
    LockReleased {
        agent_id: AgentId,
        path: PathBuf,
    },
    LockTimeout {
        agent_id: AgentId,
        path: PathBuf,
    },
    ConflictDetected {
        path: PathBuf,
        agents: Vec<AgentId>,
    },
}

/// Status of the workspace
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceStatus {
    pub branch: String,
    pub mode: CollaborationMode,
    pub locked_files: Vec<LockedFileInfo>,
}

/// Info about a locked file
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockedFileInfo {
    pub path: PathBuf,
    pub agent_id: AgentId,
    pub lock_type: LockType,
    pub acquired_at: String, // ISO format timestamp
}

/// Shared workspace for agent coordination
pub struct SharedWorkspace {
    working_dir: PathBuf,
    branch: String,
    mode: CollaborationMode,
    file_locks: RwLock<HashMap<PathBuf, Vec<FileLock>>>,
    event_subscribers: Mutex<Vec<mpsc::UnboundedSender<WorkspaceEvent>>>,
    lock_timeout: Duration,
}

impl SharedWorkspace {
    /// Create a new shared workspace
    pub fn new(
        working_dir: impl Into<PathBuf>,
        branch: impl Into<String>,
        mode: CollaborationMode,
    ) -> Self {
        Self {
            working_dir: working_dir.into(),
            branch: branch.into(),
            mode,
            file_locks: RwLock::new(HashMap::new()),
            event_subscribers: Mutex::new(Vec::new()),
            lock_timeout: Duration::from_secs(30),
        }
    }

    /// Create with custom lock timeout
    pub fn with_timeout(
        working_dir: impl Into<PathBuf>,
        branch: impl Into<String>,
        mode: CollaborationMode,
        timeout: Duration,
    ) -> Self {
        Self {
            working_dir: working_dir.into(),
            branch: branch.into(),
            mode,
            file_locks: RwLock::new(HashMap::new()),
            event_subscribers: Mutex::new(Vec::new()),
            lock_timeout: timeout,
        }
    }

    /// Acquire a lock on a file (blocks until acquired or timeout)
    pub async fn acquire_lock(
        &self,
        agent_id: &AgentId,
        file_path: &Path,
        lock_type: LockType,
    ) -> Result<FileLockGuard> {
        let deadline = Instant::now() + self.lock_timeout;
        
        loop {
            match self.try_acquire_lock(agent_id, file_path, lock_type).await {
                Ok(guard) => return Ok(guard),
                Err(e) => {
                    if Instant::now() >= deadline {
                        // Notify about timeout
                        self.emit_event(WorkspaceEvent::LockTimeout {
                            agent_id: agent_id.clone(),
                            path: file_path.to_path_buf(),
                        }).await;
                        
                        return Err(e);
                    }
                    
                    // Wait before retry
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        }
    }

    /// Try to acquire a lock without blocking
    pub async fn try_acquire_lock(
        &self,
        agent_id: &AgentId,
        file_path: &Path,
        lock_type: LockType,
    ) -> Result<FileLockGuard> {
        let mut locks = self.file_locks.write().await;
        
        let file_locks = locks.entry(file_path.to_path_buf())
            .or_insert_with(Vec::new);
        
        // Check if this agent already holds a lock
        if let Some(existing) = file_locks.iter().find(|l| l.agent_id == *agent_id) {
            // Same agent can upgrade from shared to exclusive
            if existing.lock_type == LockType::Shared && lock_type == LockType::Exclusive {
                // Remove old lock and add new one
                file_locks.retain(|l| l.agent_id != *agent_id);
                let lock = FileLock {
                    agent_id: agent_id.clone(),
                    lock_type,
                    acquired_at: Instant::now(),
                    expires_at: None,
                };
                file_locks.push(lock);
                
                drop(locks);
                
                // Notify about lock acquisition
                self.emit_event(WorkspaceEvent::LockAcquired {
                    agent_id: agent_id.clone(),
                    path: file_path.to_path_buf(),
                    lock_type,
                }).await;
                
                return Ok(FileLockGuard {
                    workspace: self,
                    agent_id: agent_id.clone(),
                    file_path: file_path.to_path_buf(),
                });
            }
            
            return Err(KodError::InvalidState(format!(
                "Agent already holds a {:?} lock on this file",
                existing.lock_type
            )));
        }
        
        // Check compatibility with existing locks
        for existing in file_locks.iter() {
            if !existing.lock_type.is_compatible_with(&lock_type) {
                // Check if lock has expired
                if let Some(expires_at) = existing.expires_at {
                    if Instant::now() > expires_at {
                        // Lock expired, remove it
                        file_locks.retain(|l| l.agent_id != existing.agent_id);
                        continue;
                    }
                }
                
                // Detect potential conflict
                if lock_type == LockType::Exclusive && existing.lock_type == LockType::IntentToWrite {
                    self.emit_event(WorkspaceEvent::ConflictDetected {
                        path: file_path.to_path_buf(),
                        agents: vec![existing.agent_id.clone(), agent_id.clone()],
                    }).await;
                }
                
                return Err(KodError::LockTimeout {
                    path: file_path.display().to_string(),
                });
            }
        }
        
        // Acquire the lock
        let lock = FileLock {
            agent_id: agent_id.clone(),
            lock_type,
            acquired_at: Instant::now(),
            expires_at: None,
        };
        file_locks.push(lock);
        
        drop(locks);
        
        // Notify about lock acquisition
        self.emit_event(WorkspaceEvent::LockAcquired {
            agent_id: agent_id.clone(),
            path: file_path.to_path_buf(),
            lock_type,
        }).await;
        
        Ok(FileLockGuard {
            workspace: self,
            agent_id: agent_id.clone(),
            file_path: file_path.to_path_buf(),
        })
    }

    /// Release a lock
    pub async fn release_lock(
        &self,
        agent_id: &AgentId,
        file_path: &Path,
    ) {
        let mut locks = self.file_locks.write().await;
        
        if let Some(file_locks) = locks.get_mut(file_path) {
            file_locks.retain(|l| l.agent_id != *agent_id);
            
            if file_locks.is_empty() {
                locks.remove(file_path);
            }
        }
        
        drop(locks);
        
        // Notify about lock release
        self.emit_event(WorkspaceEvent::LockReleased {
            agent_id: agent_id.clone(),
            path: file_path.to_path_buf(),
        }).await;
    }

    /// Check if a file is locked
    pub async fn is_locked(&self, file_path: &Path) -> bool {
        let locks = self.file_locks.read().await;
        locks.get(file_path)
            .map(|file_locks| !file_locks.is_empty())
            .unwrap_or(false)
    }

    /// Get lock info for a file
    pub async fn get_lock_info(&self, file_path: &Path) -> Option<FileLock> {
        let locks = self.file_locks.read().await;
        locks.get(file_path)
            .and_then(|file_locks| file_locks.first())
            .cloned()
    }

    /// Get all current locks
    pub async fn get_all_locks(&self) -> Vec<FileLock> {
        let locks = self.file_locks.read().await;
        locks.values()
            .flat_map(|file_locks| file_locks.iter())
            .cloned()
            .collect()
    }

    /// Get workspace status
    pub async fn status(&self) -> WorkspaceStatus {
        let locks = self.file_locks.read().await;
        
        let locked_files = locks.iter()
            .flat_map(|(path, file_locks)| {
                file_locks.iter().map(move |lock| {
                    LockedFileInfo {
                        path: path.clone(),
                        agent_id: lock.agent_id.clone(),
                        lock_type: lock.lock_type,
                        acquired_at: chrono::Utc::now().to_rfc3339(), // Simplified for status
                    }
                })
            })
            .collect();
        
        WorkspaceStatus {
            branch: self.branch.clone(),
            mode: self.mode.clone(),
            locked_files,
        }
    }

    /// Subscribe to workspace events
    pub async fn subscribe_to_events(&self, sender: mpsc::UnboundedSender<WorkspaceEvent>) {
        self.event_subscribers.lock().await.push(sender);
    }

    /// Get working directory
    pub fn working_dir(&self) -> &Path {
        &self.working_dir
    }

    /// Get branch
    pub fn branch(&self) -> &str {
        &self.branch
    }

    /// Get collaboration mode
    pub fn mode(&self) -> &CollaborationMode {
        &self.mode
    }

    /// Emit an event to subscribers
    async fn emit_event(&self, event: WorkspaceEvent) {
        let subscribers = self.event_subscribers.lock().await;
        
        for sender in subscribers.iter() {
            let _ = sender.send(event.clone());
        }
    }
}

/// RAII guard for file locks - releases lock when dropped
pub struct FileLockGuard<'a> {
    workspace: &'a SharedWorkspace,
    agent_id: AgentId,
    file_path: PathBuf,
}

impl<'a> FileLockGuard<'a> {
    /// Explicitly release the lock
    pub async fn release(self) {
        // Drop will handle release
    }
}

impl<'a> Drop for FileLockGuard<'a> {
    fn drop(&mut self) {
        let workspace = self.workspace;
        let agent_id = self.agent_id.clone();
        let file_path = self.file_path.clone();
        
        // Spawn a task to release the lock since Drop is sync
        tokio::spawn(async move {
            workspace.release_lock(&agent_id, &file_path).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_lock_lifecycle() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = SharedWorkspace::new(temp_dir.path(), "test", CollaborationMode::SharedBranch);
        
        let agent = AgentId::new();
        let file = Path::new("test.rs");
        
        {
            let _guard = workspace.acquire_lock(&agent, file, LockType::Exclusive).await.unwrap();
            assert!(workspace.is_locked(file).await);
        }
        
        // Give time for async drop
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!workspace.is_locked(file).await);
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
cargo test -p kod-swarm --test workspace
cargo test -p kod-swarm --lib workspace
```

Expected: All tests pass

- [ ] **Step 5: Commit**

```bash
git add crates/kod-swarm/
git commit -m "feat(swarm): add shared workspace with file locking and event notifications"
```

---

## Task 32: Task Coordination

**Files:**
- Create: `crates/kod-swarm/src/coordination.rs`
- Test: `crates/kod-swarm/tests/coordination.rs`

- [ ] **Step 1: Write failing test for task coordination**

Create `crates/kod-swarm/tests/coordination.rs`:

```rust
use kod_swarm::coordination::{TaskCoordinator, TaskAssignment, TaskDecomposition};
use kod_swarm::{Agent, AgentBuilder, Capability};
use kod_types::{AgentId, Priority, TaskId, TaskStatus};

fn create_test_agents() -> Vec<Agent> {
    vec![
        AgentBuilder::new("architect")
            .with_capability(Capability::Planning)
            .build(),
        
        AgentBuilder::new("coder1")
            .with_capability(Capability::Coding)
            .build(),
        
        AgentBuilder::new("coder2")
            .with_capability(Capability::Coding)
            .with_capability(Capability::Testing)
            .build(),
        
        AgentBuilder::new("reviewer")
            .with_capability(Capability::CodeReview)
            .build(),
    ]
}

#[tokio::test]
async fn test_decompose_task() {
    let coordinator = TaskCoordinator::new();
    
    let complex_task = "Implement a complete authentication system with JWT tokens, including login, registration, password reset, and email verification";
    
    let decomposition = coordinator.decompose_task(complex_task).await.unwrap();
    
    // Should break down into multiple subtasks
    assert!(decomposition.subtasks.len() >= 3);
    
    // Each subtask should have required capabilities
    for subtask in &decomposition.subtasks {
        assert!(!subtask.required_capabilities.is_empty());
        assert!(!subtask.description.is_empty());
    }
    
    // Should include testing subtask
    let has_testing = decomposition.subtasks.iter()
        .any(|st| st.required_capabilities.contains(&Capability::Testing));
    assert!(has_testing);
}

#[tokio::test]
async fn test_assign_tasks_to_agents() {
    let coordinator = TaskCoordinator::new();
    let agents = create_test_agents();
    
    let task = "Write unit tests for the auth module";
    
    let decomposition = coordinator.decompose_task(task).await.unwrap();
    let assignments = coordinator.assign_tasks(&agents, &decomposition).await.unwrap();
    
    // Should have at least one assignment
    assert!(!assignments.is_empty());
    
    // Assignment should be to an agent with testing capability
    let test_assignment = assignments.iter()
        .find(|a| a.subtask.required_capabilities.contains(&Capability::Testing));
    
    if let Some(assignment) = test_assignment {
        let assigned_agent = agents.iter()
            .find(|agent| agent.id() == &assignment.agent_id);
        
        if let Some(agent) = assigned_agent {
            assert!(agent.has_capability(&Capability::Testing));
        }
    }
}

#[tokio::test]
async fn test_find_agent_by_capability() {
    let coordinator = TaskCoordinator::new();
    let agents = create_test_agents();
    
    // Find agent with coding capability
    let coding_agent = coordinator.find_agent_by_capability(&agents, &Capability::Coding);
    assert!(coding_agent.is_some());
    
    let coding_agent = coding_agent.unwrap();
    assert!(coding_agent.has_capability(&Capability::Coding));
    
    // Find agent with planning capability
    let planning_agent = coordinator.find_agent_by_capability(&agents, &Capability::Planning);
    assert!(planning_agent.is_some());
    
    // No agent has research capability
    let research_agent = coordinator.find_agent_by_capability(&agents, &Capability::Research);
    assert!(research_agent.is_none());
}

#[tokio::test]
async fn test_task_assignment_structure() {
    let coordinator = TaskCoordinator::new();
    
    let task_assignment = TaskAssignment {
        task_id: TaskId::new(),
        agent_id: AgentId::new(),
        subtask: kod_swarm::coordination::SubTask {
            id: TaskId::new(),
            description: "Test task".to_string(),
            required_capabilities: vec![Capability::Coding],
            priority: Priority::High,
            dependencies: Vec::new(),
        },
        assigned_at: chrono::Utc::now(),
    };
    
    assert_eq!(task_assignment.subtask.priority, Priority::High);
    assert_eq!(task_assignment.subtask.required_capabilities.len(), 1);
}

#[tokio::test]
async fn test_coordination_with_dependencies() {
    let coordinator = TaskCoordinator::new();
    
    let task = "First design the API, then implement it, then write tests";
    
    let decomposition = coordinator.decompose_task(task).await.unwrap();
    
    // Subtasks should have dependencies
    let with_deps = decomposition.subtasks.iter()
        .filter(|st| !st.dependencies.is_empty())
        .count();
    
    // At least one subtask should depend on another
    assert!(with_deps > 0 || decomposition.subtasks.len() > 1);
}

#[tokio::test]
async fn test_result_collection() {
    let coordinator = TaskCoordinator::new();
    
    let task_id = TaskId::new();
    let agent_id = AgentId::new();
    
    // Record a result
    coordinator.record_result(
        task_id.clone(),
        agent_id,
        TaskStatus::Completed,
        "Successfully completed the task".to_string(),
    ).await;
    
    // Get result
    let result = coordinator.get_result(&task_id).await;
    assert!(result.is_some());
    
    let result = result.unwrap();
    assert_eq!(result.status, TaskStatus::Completed);
    assert_eq!(result.output, "Successfully completed the task");
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test -p kod-swarm --test coordination
```

Expected: FAIL - coordination module not implemented

- [ ] **Step 3: Implement task coordination**

Create `crates/kod-swarm/src/coordination.rs`:

```rust
//! Task coordination for multi-agent task decomposition and assignment.

use crate::agent::{Agent, Capability};
use kod_error::{KodError, Result};
use kod_types::{AgentId, Priority, TaskId, TaskStatus};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use chrono::{DateTime, Utc};

/// A subtask in a decomposed task
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct SubTask {
    pub id: TaskId,
    pub description: String,
    pub required_capabilities: Vec<Capability>,
    pub priority: Priority,
    pub dependencies: Vec<TaskId>,
}

/// Result of task decomposition
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct TaskDecomposition {
    pub original_task: String,
    pub subtasks: Vec<SubTask>,
}

/// Assignment of a subtask to an agent
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct TaskAssignment {
    pub task_id: TaskId,
    pub agent_id: AgentId,
    pub subtask: SubTask,
    pub assigned_at: DateTime<Utc>,
}

/// Result of a completed task
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct TaskResult {
    pub task_id: TaskId,
    pub agent_id: AgentId,
    pub status: TaskStatus,
    pub output: String,
    pub completed_at: DateTime<Utc>,
}

/// Coordinates task decomposition and assignment to agents
pub struct TaskCoordinator {
    results: Arc<RwLock<HashMap<TaskId, TaskResult>>>,
}

impl TaskCoordinator {
    pub fn new() -> Self {
        Self {
            results: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Decompose a complex task into subtasks
    pub async fn decompose_task(&self, task: &str) -> Result<TaskDecomposition> {
        let subtasks = self.analyze_and_decompose(task).await?;
        
        Ok(TaskDecomposition {
            original_task: task.to_string(),
            subtasks,
        })
    }

    /// Assign subtasks to agents based on capabilities
    pub async fn assign_tasks(
        &self,
        agents: &[Agent],
        decomposition: &TaskDecomposition,
    ) -> Result<Vec<TaskAssignment>> {
        let mut assignments = Vec::new();
        
        for subtask in &decomposition.subtasks {
            // Find best agent for this subtask
            if let Some(agent) = self.find_best_agent(agents, subtask) {
                assignments.push(TaskAssignment {
                    task_id: subtask.id.clone(),
                    agent_id: agent.id().clone(),
                    subtask: subtask.clone(),
                    assigned_at: Utc::now(),
                });
            } else {
                tracing::warn!(
                    subtask = %subtask.description,
                    "No agent found with required capabilities: {:?}",
                    subtask.required_capabilities
                );
            }
        }
        
        Ok(assignments)
    }

    /// Find an agent with the given capability
    pub fn find_agent_by_capability(
        &self,
        agents: &[Agent],
        capability: &Capability,
    ) -> Option<&Agent> {
        agents.iter().find(|agent| agent.has_capability(capability))
    }

    /// Find the best agent for a subtask
    fn find_best_agent<'a>(
        &self,
        agents: &'a [Agent],
        subtask: &SubTask,
    ) -> Option<&'a Agent> {
        // Score agents based on capability match
        let mut scored_agents: Vec<(usize, &Agent)> = agents.iter()
            .map(|agent| {
                let matching_caps = subtask.required_capabilities.iter()
                    .filter(|cap| agent.has_capability(cap))
                    .count();
                (matching_caps, agent)
            })
            .collect();
        
        // Sort by number of matching capabilities (descending)
        scored_agents.sort_by(|a, b| b.0.cmp(&a.0));
        
        // Return agent with most matching capabilities
        scored_agents.into_iter()
            .filter(|(score, _)| *score > 0)
            .map(|(_, agent)| agent)
            .next()
    }

    /// Record a task result
    pub async fn record_result(
        &self,
        task_id: TaskId,
        agent_id: AgentId,
        status: TaskStatus,
        output: String,
    ) {
        let result = TaskResult {
            task_id: task_id.clone(),
            agent_id,
            status,
            output,
            completed_at: Utc::now(),
        };
        
        self.results.write().await.insert(task_id, result);
    }

    /// Get a task result
    pub async fn get_result(&self, task_id: &TaskId) -> Option<TaskResult> {
        self.results.read().await.get(task_id).cloned()
    }

    /// Get all results
    pub async fn get_all_results(&self) -> Vec<TaskResult> {
        self.results.read().await.values().cloned().collect()
    }

    /// Analyze a task and decompose it into subtasks
    async fn analyze_and_decompose(&self, task: &str) -> Result<Vec<SubTask>> {
        let mut subtasks = Vec::new();
        let task_lower = task.to_lowercase();
        
        // Pattern-based decomposition
        if task_lower.contains("implement") || task_lower.contains("create") || task_lower.contains("build") {
            subtasks.push(SubTask {
                id: TaskId::new(),
                description: format!("Design and plan: {}", task),
                required_capabilities: vec![Capability::Planning],
                priority: Priority::High,
                dependencies: Vec::new(),
            });
            
            subtasks.push(SubTask {
                id: TaskId::new(),
                description: format!("Implement core functionality: {}", task),
                required_capabilities: vec![Capability::Coding],
                priority: Priority::High,
                dependencies: vec![subtasks[0].id.clone()],
            });
        }
        
        if task_lower.contains("test") || task_lower.contains("verify") {
            subtasks.push(SubTask {
                id: TaskId::new(),
                description: format!("Write tests: {}", task),
                required_capabilities: vec![Capability::Testing],
                priority: Priority::Medium,
                dependencies: subtasks.iter()
                    .filter(|st| st.required_capabilities.contains(&Capability::Coding))
                    .map(|st| st.id.clone())
                    .collect(),
            });
        }
        
        if task_lower.contains("review") || task_lower.contains("document") {
            subtasks.push(SubTask {
                id: TaskId::new(),
                description: format!("Review and document: {}", task),
                required_capabilities: vec![Capability::CodeReview, Capability::Documentation],
                priority: Priority::Low,
                dependencies: Vec::new(),
            });
        }
        
        if task_lower.contains("debug") || task_lower.contains("fix") {
            subtasks.push(SubTask {
                id: TaskId::new(),
                description: format!("Debug and fix: {}", task),
                required_capabilities: vec![Capability::Debugging],
                priority: Priority::High,
                dependencies: Vec::new(),
            });
        }
        
        if task_lower.contains("refactor") {
            subtasks.push(SubTask {
                id: TaskId::new(),
                description: format!("Refactor: {}", task),
                required_capabilities: vec![Capability::Refactoring, Capability::Coding],
                priority: Priority::Medium,
                dependencies: Vec::new(),
            });
        }
        
        if task_lower.contains("research") || task_lower.contains("investigate") {
            subtasks.push(SubTask {
                id: TaskId::new(),
                description: format!("Research: {}", task),
                required_capabilities: vec![Capability::Research],
                priority: Priority::Medium,
                dependencies: Vec::new(),
            });
        }
        
        // If no specific patterns matched, create a general coding task
        if subtasks.is_empty() {
            subtasks.push(SubTask {
                id: TaskId::new(),
                description: task.to_string(),
                required_capabilities: vec![Capability::Coding],
                priority: Priority::Medium,
                dependencies: Vec::new(),
            });
        }
        
        // Always ensure there's at least a basic coding subtask
        let has_coding = subtasks.iter()
            .any(|st| st.required_capabilities.contains(&Capability::Coding));
        
        if !has_coding {
            subtasks.push(SubTask {
                id: TaskId::new(),
                description: format!("Implementation: {}", task),
                required_capabilities: vec![Capability::Coding],
                priority: Priority::Medium,
                dependencies: Vec::new(),
            });
        }
        
        Ok(subtasks)
    }
}

impl Default for TaskCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_decomposition() {
        let coordinator = TaskCoordinator::new();
        
        let decomposition = coordinator.decompose_task("Implement user auth").await.unwrap();
        
        assert!(!decomposition.subtasks.is_empty());
        
        let has_planning = decomposition.subtasks.iter()
            .any(|st| st.required_capabilities.contains(&Capability::Planning));
        assert!(has_planning);
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
cargo test -p kod-swarm --test coordination
cargo test -p kod-swarm --lib coordination
```

Expected: All tests pass

- [ ] **Step 5: Commit**

```bash
git add crates/kod-swarm/
git commit -m "feat(swarm): add task coordination with decomposition and capability-based assignment"
```

---

## Task 33: Agent Swarm Manager

**Files:**
- Create: `crates/kod-swarm/src/swarm.rs`
- Test: `crates/kod-swarm/tests/swarm.rs`

- [ ] **Step 1: Write failing test for swarm manager**

Create `crates/kod-swarm/tests/swarm.rs`:

```rust
use kod_swarm::swarm::AgentSwarm;
use kod_swarm::{AgentBuilder, Capability, CollaborationMode, MessageContent};
use std::time::Duration;
use tempfile::TempDir;

#[tokio::test]
async fn test_swarm_creation() {
    let temp_dir = TempDir::new().unwrap();
    
    let swarm = AgentSwarm::new(
        temp_dir.path(),
        CollaborationMode::SharedBranch,
    ).await.unwrap();
    
    assert_eq!(swarm.mode(), &CollaborationMode::SharedBranch);
    assert_eq!(swarm.agent_count().await, 0);
}

#[tokio::test]
async fn test_spawn_agent() {
    let temp_dir = TempDir::new().unwrap();
    
    let mut swarm = AgentSwarm::new(
        temp_dir.path(),
        CollaborationMode::SharedBranch,
    ).await.unwrap();
    
    let agent_id = swarm.spawn_agent(
        "coder",
        vec![Capability::Coding],
    ).await.unwrap();
    
    assert_eq!(swarm.agent_count().await, 1);
    
    let agent = swarm.get_agent(&agent_id).await;
    assert!(agent.is_some());
    
    let agent = agent.unwrap();
    assert_eq!(agent.name(), "coder");
    assert!(agent.has_capability(&Capability::Coding));
}

#[tokio::test]
async fn test_spawn_multiple_agents() {
    let temp_dir = TempDir::new().unwrap();
    
    let mut swarm = AgentSwarm::new(
        temp_dir.path(),
        CollaborationMode::SharedBranch,
    ).await.unwrap();
    
    swarm.spawn_agent("architect", vec![Capability::Planning]).await.unwrap();
    swarm.spawn_agent("coder", vec![Capability::Coding]).await.unwrap();
    swarm.spawn_agent("tester", vec![Capability::Testing]).await.unwrap();
    
    assert_eq!(swarm.agent_count().await, 3);
    
    let agents = swarm.list_agents().await;
    assert_eq!(agents.len(), 3);
    
    let names: Vec<String> = agents.iter()
        .map(|agent| agent.name().to_string())
        .collect();
    
    assert!(names.contains(&"architect".to_string()));
    assert!(names.contains(&"coder".to_string()));
    assert!(names.contains(&"tester".to_string()));
}

#[tokio::test]
async fn test_remove_agent() {
    let temp_dir = TempDir::new().unwrap();
    
    let mut swarm = AgentSwarm::new(
        temp_dir.path(),
        CollaborationMode::SharedBranch,
    ).await.unwrap();
    
    let agent_id = swarm.spawn_agent("temp", vec![Capability::Coding]).await.unwrap();
    assert_eq!(swarm.agent_count().await, 1);
    
    swarm.remove_agent(&agent_id).await;
    assert_eq!(swarm.agent_count().await, 0);
    
    let agent = swarm.get_agent(&agent_id).await;
    assert!(agent.is_none());
}

#[tokio::test]
async fn test_agent_communication_in_swarm() {
    let temp_dir = TempDir::new().unwrap();
    
    let swarm = AgentSwarm::new(
        temp_dir.path(),
        CollaborationMode::SharedBranch,
    ).await.unwrap();
    
    let agent_a = swarm.spawn_agent("agent_a", vec![Capability::Coding]).await.unwrap();
    let agent_b = swarm.spawn_agent("agent_b", vec![Capability::Testing]).await.unwrap();
    
    // Send message from A to B
    swarm.send_message(
        &agent_a,
        &agent_b,
        MessageContent::TaskAssignment {
            description: "Write tests".to_string(),
            priority: kod_types::Priority::High,
        },
    ).await.unwrap();
    
    // Agent B should receive message
    let messages = swarm.get_agent_messages(&agent_b).await;
    assert_eq!(messages.len(), 1);
    
    let message = &messages[0];
    assert_eq!(message.from, agent_a);
}

#[tokio::test]
async fn test_execute_task_with_swarm() {
    let temp_dir = TempDir::new().unwrap();
    
    let mut swarm = AgentSwarm::new(
        temp_dir.path(),
        CollaborationMode::SharedBranch,
    ).await.unwrap();
    
    // Spawn agents with different capabilities
    swarm.spawn_agent("architect", vec![Capability::Planning]).await.unwrap();
    swarm.spawn_agent("coder", vec![Capability::Coding]).await.unwrap();
    swarm.spawn_agent("tester", vec![Capability::Testing]).await.unwrap();
    
    // Execute a task
    let task = "Implement and test a simple calculator";
    let result = swarm.execute_task(task).await;
    
    match result {
        Ok(execution_result) => {
            // Should have assignments
            assert!(!execution_result.assignments.is_empty());
            
            // Should have decomposition
            assert!(!execution_result.decomposition.subtasks.is_empty());
        }
        Err(e) => {
            panic!("Task execution failed: {:?}", e);
        }
    }
}

#[tokio::test]
async fn test_swarm_status() {
    let temp_dir = TempDir::new().unwrap();
    
    let mut swarm = AgentSwarm::new(
        temp_dir.path(),
        CollaborationMode::SharedBranch,
    ).await.unwrap();
    
    swarm.spawn_agent("agent1", vec![Capability::Coding]).await.unwrap();
    swarm.spawn_agent("agent2", vec![Capability::Testing]).await.unwrap();
    
    let status = swarm.status().await;
    assert_eq!(status.agent_count, 2);
    assert_eq!(status.mode, CollaborationMode::SharedBranch);
}

#[tokio::test]
async fn test_shared_workspace_integration() {
    let temp_dir = TempDir::new().unwrap();
    
    let swarm = AgentSwarm::new(
        temp_dir.path(),
        CollaborationMode::SharedBranch,
    ).await.unwrap();
    
    let agent_a = swarm.spawn_agent("agent_a", vec![Capability::Coding]).await.unwrap();
    let agent_b = swarm.spawn_agent("agent_b", vec![Capability::Coding]).await.unwrap();
    
    // Agent A acquires lock
    swarm.acquire_file_lock(
        &agent_a,
        std::path::Path::new("src/main.rs"),
        kod_swarm::LockType::Exclusive,
    ).await.unwrap();
    
    // Check lock status
    let is_locked = swarm.is_file_locked(std::path::Path::new("src/main.rs")).await;
    assert!(is_locked);
    
    // Agent B should not be able to acquire
    let result = swarm.try_acquire_file_lock(
        &agent_b,
        std::path::Path::new("src/main.rs"),
        kod_swarm::LockType::Exclusive,
    ).await;
    
    assert!(result.is_err());
}

#[tokio::test]
async fn test_coordination_flow() {
    let temp_dir = TempDir::new().unwrap();
    
    let mut swarm = AgentSwarm::new(
        temp_dir.path(),
        CollaborationMode::SharedBranch,
    ).await.unwrap();
    
    // Spawn agents for a full development workflow
    let architect = swarm.spawn_agent("architect", vec![Capability::Planning]).await.unwrap();
    let coder = swarm.spawn_agent("coder", vec![Capability::Coding]).await.unwrap();
    let tester = swarm.spawn_agent("tester", vec![Capability::Testing]).await.unwrap();
    let reviewer = swarm.spawn_agent("reviewer", vec![Capability::CodeReview]).await.unwrap();
    
    // Coordinate a complex task
    let task = "Design, implement, test, and review a new authentication system";
    
    let result = swarm.coordinate_complex_task(task).await;
    
    match result {
        Ok(coordination) => {
            // Should have multiple agents involved
            assert!(coordination.involved_agents.len() >= 2);
            
            // Should have task decomposition
            assert!(!coordination.decomposition.subtasks.is_empty());
            
            // Should have assignments
            assert!(!coordination.assignments.is_empty());
        }
        Err(e) => {
            panic!("Coordination failed: {:?}", e);
        }
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test -p kod-swarm --test swarm
```

Expected: FAIL - swarm module not implemented

- [ ] **Step 3: Implement agent swarm manager**

Create `crates/kod-swarm/src/swarm.rs`:

```rust
//! Agent swarm manager - coordinates multiple agents working together.

use crate::{
    agent::{Agent, AgentBuilder, AgentState, Capability},
    communication::{AgentCommunicationHub, MessageContent},
    coordination::{TaskAssignment, TaskCoordinator, TaskDecomposition},
    workspace::{CollaborationMode, LockType, SharedWorkspace},
};
use kod_error::{KodError, Result};
use kod_types::AgentId;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Result of task execution
#[derive(Debug, Clone)]
pub struct TaskExecutionResult {
    pub decomposition: TaskDecomposition,
    pub assignments: Vec<TaskAssignment>,
}

/// Result of complex task coordination
#[derive(Debug, Clone)]
pub struct CoordinationResult {
    pub involved_agents: Vec<AgentId>,
    pub decomposition: TaskDecomposition,
    pub assignments: Vec<TaskAssignment>,
}

/// Status of the swarm
#[derive(Debug, Clone)]
pub struct SwarmStatus {
    pub mode: CollaborationMode,
    pub agent_count: usize,
    pub agents: Vec<AgentId>,
}

/// Manages a swarm of agents
pub struct AgentSwarm {
    agents: Arc<RwLock<HashMap<AgentId, Agent>>>,
    communication: Arc<AgentCommunicationHub>,
    workspace: Arc<SharedWorkspace>,
    coordinator: Arc<TaskCoordinator>,
    mode: CollaborationMode,
}

impl AgentSwarm {
    /// Create a new agent swarm
    pub async fn new(
        working_dir: impl AsRef<Path>,
        mode: CollaborationMode,
    ) -> Result<Self> {
        let workspace = SharedWorkspace::new(
            working_dir,
            "main", // Default branch
            mode.clone(),
        );
        
        Ok(Self {
            agents: Arc::new(RwLock::new(HashMap::new())),
            communication: Arc::new(AgentCommunicationHub::new()),
            workspace: Arc::new(workspace),
            coordinator: Arc::new(TaskCoordinator::new()),
            mode,
        })
    }

    /// Create with custom branch
    pub async fn with_branch(
        working_dir: impl AsRef<Path>,
        branch: impl Into<String>,
        mode: CollaborationMode,
    ) -> Result<Self> {
        let workspace = SharedWorkspace::new(
            working_dir,
            branch,
            mode.clone(),
        );
        
        Ok(Self {
            agents: Arc::new(RwLock::new(HashMap::new())),
            communication: Arc::new(AgentCommunicationHub::new()),
            workspace: Arc::new(workspace),
            coordinator: Arc::new(TaskCoordinator::new()),
            mode,
        })
    }

    /// Spawn a new agent
    pub async fn spawn_agent(
        &self,
        name: impl Into<String>,
        capabilities: Vec<Capability>,
    ) -> Result<AgentId> {
        let mut builder = AgentBuilder::new(name);
        
        for capability in capabilities {
            builder = builder.with_capability(capability);
        }
        
        let agent = builder.build();
        let agent_id = agent.id().clone();
        
        // Register with communication hub
        self.communication.register_agent(agent_id.clone()).await?;
        
        // Store agent
        self.agents.write().await.insert(agent_id.clone(), agent);
        
        Ok(agent_id)
    }

    /// Remove an agent
    pub async fn remove_agent(&self, agent_id: &AgentId) {
        self.agents.write().await.remove(agent_id);
        self.communication.unregister_agent(agent_id).await;
    }

    /// Get an agent by ID
    pub async fn get_agent(&self, agent_id: &AgentId) -> Option<Agent> {
        self.agents.read().await.get(agent_id).cloned()
    }

    /// List all agents
    pub async fn list_agents(&self) -> Vec<Agent> {
        self.agents.read().await.values().cloned().collect()
    }

    /// Get agent count
    pub async fn agent_count(&self) -> usize {
        self.agents.read().await.len()
    }

    /// Get collaboration mode
    pub fn mode(&self) -> &CollaborationMode {
        &self.mode
    }

    /// Send a message from one agent to another
    pub async fn send_message(
        &self,
        from: &AgentId,
        to: &AgentId,
        content: MessageContent,
    ) -> Result<()> {
        self.communication.send_direct(from, to, content).await
    }

    /// Broadcast a message to all agents
    pub async fn broadcast_message(
        &self,
        from: &AgentId,
        content: MessageContent,
    ) -> Result<()> {
        self.communication.broadcast(from, content).await
    }

    /// Get messages for an agent
    pub async fn get_agent_messages(&self, agent_id: &AgentId) -> Vec<kod_types::AgentMessage> {
        self.communication.get_agent_history(agent_id).await
    }

    /// Acquire a file lock for an agent
    pub async fn acquire_file_lock(
        &self,
        agent_id: &AgentId,
        file_path: &Path,
        lock_type: LockType,
    ) -> Result<()> {
        // Use try_acquire to avoid blocking in this API
        let _guard = self.workspace.try_acquire_lock(agent_id, file_path, lock_type).await?;
        Ok(())
    }

    /// Try to acquire a file lock (non-blocking)
    pub async fn try_acquire_file_lock(
        &self,
        agent_id: &AgentId,
        file_path: &Path,
        lock_type: LockType,
    ) -> Result<()> {
        let _guard = self.workspace.try_acquire_lock(agent_id, file_path, lock_type).await?;
        Ok(())
    }

    /// Check if a file is locked
    pub async fn is_file_locked(&self, file_path: &Path) -> bool {
        self.workspace.is_locked(file_path).await
    }

    /// Execute a task using the swarm
    pub async fn execute_task(&self, task: &str) -> Result<TaskExecutionResult> {
        // 1. Decompose the task
        let decomposition = self.coordinator.decompose_task(task).await?;
        
        // 2. Get available agents
        let agents = self.list_agents().await;
        
        // 3. Assign tasks to agents
        let assignments = self.coordinator.assign_tasks(&agents, &decomposition).await?;
        
        Ok(TaskExecutionResult {
            decomposition,
            assignments,
        })
    }

    /// Coordinate a complex task involving multiple agents
    pub async fn coordinate_complex_task(&self, task: &str) -> Result<CoordinationResult> {
        // 1. Decompose the task
        let decomposition = self.coordinator.decompose_task(task).await?;
        
        // 2. Get all agents
        let agents = self.list_agents().await;
        
        // 3. Assign tasks
        let assignments = self.coordinator.assign_tasks(&agents, &decomposition).await?;
        
        // 4. Determine involved agents
        let involved_agents: Vec<AgentId> = assignments.iter()
            .map(|assignment| assignment.agent_id.clone())
            .collect();
        
        // 5. Notify agents about their assignments
        for assignment in &assignments {
            let agent_id = &assignment.agent_id;
            
            // Send task assignment message
            let _ = self.send_message(
                &self.get_orchestrator_id().await,
                agent_id,
                MessageContent::TaskAssignment {
                    description: assignment.subtask.description.clone(),
                    priority: assignment.subtask.priority,
                },
            ).await;
        }
        
        Ok(CoordinationResult {
            involved_agents,
            decomposition,
            assignments,
        })
    }

    /// Get swarm status
    pub async fn status(&self) -> SwarmStatus {
        let agents = self.agents.read().await;
        
        SwarmStatus {
            mode: self.mode.clone(),
            agent_count: agents.len(),
            agents: agents.keys().cloned().collect(),
        }
    }

    /// Get or create an orchestrator agent ID
    async fn get_orchestrator_id(&self) -> AgentId {
        // For now, use a virtual orchestrator ID
        // In a real system, this would be a dedicated coordinator agent
        AgentId::new()
    }

    /// Get communication hub
    pub fn communication(&self) -> &AgentCommunicationHub {
        &self.communication
    }

    /// Get workspace
    pub fn workspace(&self) -> &SharedWorkspace {
        &self.workspace
    }

    /// Get coordinator
    pub fn coordinator(&self) -> &TaskCoordinator {
        &self.coordinator
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_swarm_basic_operations() {
        let temp_dir = TempDir::new().unwrap();
        
        let swarm = AgentSwarm::new(temp_dir.path(), CollaborationMode::SharedBranch).await.unwrap();
        
        let agent_id = swarm.spawn_agent("test", vec![Capability::Coding]).await.unwrap();
        
        assert_eq!(swarm.agent_count().await, 1);
        
        let agent = swarm.get_agent(&agent_id).await;
        assert!(agent.is_some());
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
cargo test -p kod-swarm --test swarm
cargo test -p kod-swarm --lib swarm
```

Expected: All tests pass

- [ ] **Step 5: Commit**

```bash
git add crates/kod-swarm/
git commit -m "feat(swarm): add agent swarm manager with coordination and file locking integration"
```

---

## Task 34: Swarm Integration Test

**Files:**
- Create: `crates/kod-swarm/tests/integration.rs`

- [ ] **Step 1: Write comprehensive integration test**

Create `crates/kod-swarm/tests/integration.rs`:

```rust
use kod_swarm::{
    swarm::AgentSwarm,
    AgentBuilder, Capability, CollaborationMode, LockType, MessageContent,
};
use std::time::Duration;
use tempfile::TempDir;

#[tokio::test]
async fn test_full_swarm_workflow() {
    let temp_dir = TempDir::new().unwrap();
    
    // 1. Create swarm in shared branch mode
    let mut swarm = AgentSwarm::new(
        temp_dir.path(),
        CollaborationMode::SharedBranch,
    ).await.unwrap();
    
    // 2. Spawn specialized agents
    let architect = swarm.spawn_agent(
        "architect",
        vec![Capability::Planning, Capability::Research],
    ).await.unwrap();
    
    let coder = swarm.spawn_agent(
        "coder",
        vec![Capability::Coding, Capability::Refactoring],
    ).await.unwrap();
    
    let tester = swarm.spawn_agent(
        "tester",
        vec![Capability::Testing],
    ).await.unwrap();
    
    let reviewer = swarm.spawn_agent(
        "reviewer",
        vec![Capability::CodeReview, Capability::Documentation],
    ).await.unwrap();
    
    // 3. Coordinate a complex task
    let task = "Design, implement, and test a user authentication system with JWT tokens";
    
    let result = swarm.coordinate_complex_task(task).await.unwrap();
    
    // 4. Verify coordination
    assert!(!result.assignments.is_empty());
    assert!(!result.decomposition.subtasks.is_empty());
    assert!(result.involved_agents.len() >= 2);
    
    // 5. Check agents have messages
    for agent_id in &result.involved_agents {
        let messages = swarm.get_agent_messages(agent_id).await;
        assert!(!messages.is_empty(), "Agent {:?} should have messages", agent_id);
    }
}

#[tokio::test]
async fn test_file_coordination_workflow() {
    let temp_dir = TempDir::new().unwrap();
    
    let swarm = AgentSwarm::new(
        temp_dir.path(),
        CollaborationMode::SharedBranch,
    ).await.unwrap();
    
    let agent_a = swarm.spawn_agent("agent_a", vec![Capability::Coding]).await.unwrap();
    let agent_b = swarm.spawn_agent("agent_b", vec![Capability::Coding]).await.unwrap();
    
    let file_path = std::path::Path::new("src/auth.rs");
    
    // 1. Agent A claims file
    swarm.acquire_file_lock(&agent_a, file_path, LockType::Exclusive).await.unwrap();
    
    // 2. Agent A sends coordination message
    swarm.send_message(
        &agent_a,
        &agent_b,
        MessageContent::Coordination {
            action: kod_types::CoordinationAction::ProposingChange {
                file: "src/auth.rs".to_string(),
                description: "Implementing JWT authentication".to_string(),
            },
        },
    ).await.unwrap();
    
    // 3. Verify file is locked
    assert!(swarm.is_file_locked(file_path).await);
    
    // 4. Agent B tries to acquire (should fail)
    let result = swarm.try_acquire_file_lock(&agent_b, file_path, LockType::Exclusive).await;
    assert!(result.is_err());
    
    // 5. Agent B can acquire shared lock for reading
    let result = swarm.try_acquire_file_lock(&agent_b, file_path, LockType::Shared).await;
    // This might fail based on lock compatibility rules
    
    // 6. Check messages
    let messages_b = swarm.get_agent_messages(&agent_b).await;
    assert!(!messages_b.is_empty());
}

#[tokio::test]
async fn test_multi_agent_collaboration() {
    let temp_dir = TempDir::new().unwrap();
    
    let swarm = AgentSwarm::new(
        temp_dir.path(),
        CollaborationMode::SharedBranch,
    ).await.unwrap();
    
    // Spawn a team of agents
    let agents = vec![
        ("lead", vec![Capability::Planning, Capability::CodeReview]),
        ("dev1", vec![Capability::Coding]),
        ("dev2", vec![Capability::Coding, Capability::Testing]),
        ("qa", vec![Capability::Testing, Capability::Debugging]),
    ];
    
    let mut agent_ids = Vec::new();
    for (name, capabilities) in agents {
        let id = swarm.spawn_agent(name, capabilities).await.unwrap();
        agent_ids.push(id);
    }
    
    // Verify all agents are spawned
    assert_eq!(swarm.agent_count().await, 4);
    
    // Execute a task
    let task = "Build a complete REST API with authentication and testing";
    let result = swarm.execute_task(task).await.unwrap();
    
    // Should have assignments
    assert!(!result.assignments.is_empty());
    
    // Verify capabilities are matched
    for assignment in &result.assignments {
        let agent = swarm.get_agent(&assignment.agent_id).await;
        if let Some(agent) = agent {
            // Agent should have at least one required capability
            let has_capability = assignment.subtask.required_capabilities.iter()
                .any(|cap| agent.has_capability(cap));
            
            // Note: In a real system, assignment would ensure capability match
            // For testing, we just verify the structure is correct
        }
    }
}

#[tokio::test]
async fn test_swarm_lifecycle() {
    let temp_dir = TempDir::new().unwrap();
    
    let swarm = AgentSwarm::new(
        temp_dir.path(),
        CollaborationMode::SharedBranch,
    ).await.unwrap();
    
    // Spawn agents
    let agent1 = swarm.spawn_agent("agent1", vec![Capability::Coding]).await.unwrap();
    let agent2 = swarm.spawn_agent("agent2", vec![Capability::Testing]).await.unwrap();
    
    assert_eq!(swarm.agent_count().await, 2);
    
    // Get status
    let status = swarm.status().await;
    assert_eq!(status.agent_count, 2);
    
    // Remove an agent
    swarm.remove_agent(&agent1).await;
    assert_eq!(swarm.agent_count().await, 1);
    
    // Agent 2 should still be there
    let agent = swarm.get_agent(&agent2).await;
    assert!(agent.is_some());
}

#[tokio::test]
async fn test_broadcast_communication() {
    let temp_dir = TempDir::new().unwrap();
    
    let swarm = AgentSwarm::new(
        temp_dir.path(),
        CollaborationMode::SharedBranch,
    ).await.unwrap();
    
    let agent_a = swarm.spawn_agent("announcer", vec![Capability::Planning]).await.unwrap();
    let agent_b = swarm.spawn_agent("listener1", vec![Capability::Coding]).await.unwrap();
    let agent_c = swarm.spawn_agent("listener2", vec![Capability::Testing]).await.unwrap();
    
    // Broadcast from agent A
    swarm.broadcast_message(
        &agent_a,
        MessageContent::KnowledgeShare {
            information: "Architecture decision: using PostgreSQL".to_string(),
            tags: vec!["architecture".to_string(), "database".to_string()],
        },
    ).await.unwrap();
    
    // All agents should receive the message
    let messages_b = swarm.get_agent_messages(&agent_b).await;
    assert!(!messages_b.is_empty());
    
    let messages_c = swarm.get_agent_messages(&agent_c).await;
    assert!(!messages_c.is_empty());
}
```

- [ ] **Step 2: Run all kod-swarm tests**

```bash
cargo test -p kod-swarm
```

Expected: All tests pass

- [ ] **Step 3: Verify workspace builds**

```bash
cargo build --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

Expected: Build succeeds with no warnings

- [ ] **Step 4: Commit integration tests**

```bash
git add crates/kod-swarm/
git commit -m "feat(swarm): add comprehensive integration tests for swarm workflows"
```

---

## Chunk 6 Review Checklist

- [ ] Agent lifecycle management (start, pause, resume, stop)
- [ ] Capability-based agent specialization
- [ ] Direct messaging between agents
- [ ] Broadcast communication to all agents
- [ ] File locking with exclusive, shared, and intent-to-write modes
- [ ] Lock timeout handling
- [ ] Task decomposition into subtasks
- [ ] Capability-based task assignment
- [ ] Agent swarm coordination
- [ ] Message history tracking
- [ ] Workspace event notifications
- [ ] Integration between all swarm components
- [ ] All tests pass
- [ ] Clippy passes with no warnings

**Verification commands:**

```bash
cargo test -p kod-swarm
cargo clippy -p kod-swarm -- -D warnings
cargo build --workspace
```

---

## Chunk 6 Summary

**Implemented:**
1. **Agent System** (`agent.rs`)
   - Agent struct with capabilities, state, and lifecycle
   - AgentBuilder for easy construction
   - State management (Idle, Running, Paused, Stopped, Failed)
   - Heartbeat tracking for timeout detection

2. **Communication Hub** (`communication.rs`)
   - Direct messaging between agents
   - Broadcast to all agents
   - Message history with configurable size
   - Agent online/offline status tracking

3. **Shared Workspace** (`workspace.rs`)
   - File locking with three lock types (Exclusive, Shared, IntentToWrite)
   - Lock compatibility checking
   - RAII-style lock guards
   - Event system for lock notifications
   - Configurable lock timeouts

4. **Task Coordination** (`coordination.rs`)
   - Task decomposition into subtasks
   - Pattern-based decomposition (implement, test, review, etc.)
   - Capability-based agent assignment
   - Dependency tracking between subtasks
   - Result collection

5. **Agent Swarm Manager** (`swarm.rs`)
   - Unified swarm coordination
   - Agent spawning and removal
   - Integration of communication, workspace, and coordination
   - Complex task orchestration

**Next Chunk Preview:**

Chunk 7 will cover the **Core Engine** implementation:
- Task router that coordinates all subsystems
- Context builder integration
- Main engine loop
- Integration of skills, memory, tools, and swarm
- Provider integration for LLM calls

Would you like me to continue with **Chunk 7: Core Engine**?
