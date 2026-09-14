//! Agent swarm configuration.

use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
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
