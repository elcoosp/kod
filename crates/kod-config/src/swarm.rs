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

#[cfg(test)]
mod coverage_swarm_config {
    //! Every field of `SwarmConfig` has a documented default that
    //! the runner reads at startup. A regression that changed one
    //! would silently shift the runner's behaviour — a shorter
    //! timeout kills slow agents, a smaller retry budget makes a
    //! flaky local model look broken.
    use super::*;

    #[test]
    fn defaults_match_the_documented_values() {
        let c = SwarmConfig::default();
        assert_eq!(c.max_agents, 5);
        assert!(c.merge_results);
        assert_eq!(c.agent_timeout_secs, 300);
        assert_eq!(c.agent_retries, 1);
        assert_eq!(c.timeout_secs, 1800);
    }

    #[test]
    fn empty_toml_table_uses_all_defaults() {
        // The container-level `#[serde(default)]` is what lets a
        // config file with no `[swarm]` block parse.
        let c: SwarmConfig = toml::from_str("").unwrap();
        let d = SwarmConfig::default();
        assert_eq!(c.max_agents, d.max_agents);
        assert_eq!(c.merge_results, d.merge_results);
        assert_eq!(c.agent_timeout_secs, d.agent_timeout_secs);
        assert_eq!(c.agent_retries, d.agent_retries);
        assert_eq!(c.timeout_secs, d.timeout_secs);
    }

    #[test]
    fn every_field_parses_independently() {
        let c: SwarmConfig = toml::from_str("max_agents = 3").unwrap();
        assert_eq!(c.max_agents, 3);
        assert!(c.merge_results, "other fields must keep their defaults");

        let c: SwarmConfig = toml::from_str("merge_results = false").unwrap();
        assert!(!c.merge_results);
        assert_eq!(c.max_agents, 5);

        let c: SwarmConfig = toml::from_str("agent_timeout_secs = 90").unwrap();
        assert_eq!(c.agent_timeout_secs, 90);

        let c: SwarmConfig = toml::from_str("agent_retries = 3").unwrap();
        assert_eq!(c.agent_retries, 3);

        let c: SwarmConfig = toml::from_str("timeout_secs = 60").unwrap();
        assert_eq!(c.timeout_secs, 60);
    }

    #[test]
    fn zero_timeouts_disable_the_caps() {
        // The runner's documented convention: 0 means "no cap". The
        // config must preserve that value verbatim, not substitute
        // the default.
        let c: SwarmConfig = toml::from_str(
            "agent_timeout_secs = 0\ntimeout_secs = 0",
        )
        .unwrap();
        assert_eq!(c.agent_timeout_secs, 0);
        assert_eq!(c.timeout_secs, 0);
    }

    #[test]
    fn full_config_round_trips_through_toml() {
        let c = SwarmConfig {
            max_agents: 7,
            merge_results: false,
            agent_timeout_secs: 120,
            agent_retries: 2,
            timeout_secs: 600,
        };
        let toml_str = toml::to_string(&c).unwrap();
        let parsed: SwarmConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(c.max_agents, parsed.max_agents);
        assert_eq!(c.merge_results, parsed.merge_results);
        assert_eq!(c.agent_timeout_secs, parsed.agent_timeout_secs);
        assert_eq!(c.agent_retries, parsed.agent_retries);
        assert_eq!(c.timeout_secs, parsed.timeout_secs);
    }

    #[test]
    fn merge_results_defaults_to_true() {
        // The default is "yes, ask the model to merge". A regression
        // to `false` would make every swarm concatenate per-agent
        // results without synthesis, which reads as a model
        // regression to a user who does not know the flag exists.
        assert!(SwarmConfig::default().merge_results);
    }
}
