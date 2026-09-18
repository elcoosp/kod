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
    /// Per-agent wall-clock cap, in seconds. A stuck agent (a model
    /// that stalls mid-stream, a tool that never returns) is cancelled
    /// at this point and, if retries remain, restarted. Default 300.
    /// Set to 0 to disable the cap.
    pub agent_timeout_secs: u64,
    /// How many additional attempts an agent gets after a timeout or a
    /// provider error. 1 (default) means: up to two attempts total.
    pub agent_retries: u32,
    /// Overall wall-clock cap for a whole swarm run, in seconds. The
    /// per-agent cap (`agent_timeout_secs`) bounds one agent; this one
    /// bounds N agents across dependency waves. Design §D4.3 sets the
    /// default at 30 minutes. `0` disables the cap.
    pub timeout_secs: u64,
}

impl Default for SwarmConfig {
    fn default() -> Self {
        Self {
            max_agents: 5,
            merge_results: true,
            agent_timeout_secs: 300,
            agent_retries: 1,
            timeout_secs: 1800,
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
