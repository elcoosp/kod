//! Agent swarm system for coordinated multi-agent collaboration.
//!
//! This crate handles agent lifecycle, direct messaging, shared workspace
//! coordination, and task orchestration.
//!
//! # Example
//!
//! ```
//! use kod_swarm::agent::{Agent, Capability};
//!
//! let agent = Agent::new("architect")
//!     .with_capability(Capability::Planning)
//!     .build();
//!
//! assert_eq!(agent.name(), "architect");
//! ```

pub mod advisor;
pub mod agent;
pub mod agent_registry;
pub mod brief;
pub mod brief_assembly;
pub mod blackboard;
pub mod communication;
pub mod completion_report;
pub mod coordination;
pub mod file_touch;
pub mod swarm;
pub mod work_pool;

pub use agent::{Agent, AgentBuilder, AgentState, Capability, ModelConfig};
pub use blackboard::{AuthorKind, Blackboard, BlackboardEntry};
pub use communication::{
    AgentCommunicationHub, AgentMessageReceiver, MessageContent, MessageDestination, SwarmMessage,
};
pub use coordination::{TaskAssignment, TaskCoordinator};
pub use swarm::AgentSwarm;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_swarm_exports() {
        let _agent = Agent::new("test");
    }
}
