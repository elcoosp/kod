//! Agent swarm configuration.
//!
//! Minimal on purpose. The previous version typed a `[swarm]` section
//! with `default_mode`, `coordination_strategy`, and two lock timeouts
//! that no code read. This version carries only the two values the
//! swarm runner consumes.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SwarmConfig {
    /// Maximum number of agents a swarm will spawn. The decompose step
    /// targets this number; the runner clamps to `[2, 8]` because a
    /// one-agent swarm is not a swarm and eight concurrent agentic
    /// loops will saturate most local model servers.
    pub max_agents: usize,
    /// When true (default), the runner asks the model to synthesize the
    /// per-agent results into a single answer after all agents finish.
    /// When false — or when the synthesis call fails — the per-agent
    /// results are concatenated under their labels.
    pub merge_results: bool,
}

impl Default for SwarmConfig {
    fn default() -> Self {
        Self {
            max_agents: 5,
            merge_results: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_swarm_config() {
        let c = SwarmConfig::default();
        assert_eq!(c.max_agents, 5);
        assert!(c.merge_results);
    }

    #[test]
    fn test_partial_swarm_config_uses_defaults() {
        let c: SwarmConfig = toml::from_str("max_agents = 3").unwrap();
        assert_eq!(c.max_agents, 3);
        assert!(c.merge_results);
    }
}
