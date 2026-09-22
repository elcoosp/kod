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

pub mod agent;
pub mod brief;
pub mod brief_assembly;
pub mod blackboard;
pub mod communication;
pub mod coordination;
pub mod swarm;

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
