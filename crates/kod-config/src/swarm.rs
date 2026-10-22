//! Agent swarm configuration.
//!
//! Minimal on purpose. The previous version typed a `[swarm]` section
//! with `default_mode`, `coordination_strategy`, and two lock timeouts
//! that no code read. This version carries only the two values the
//! swarm runner consumes.

use serde::{Deserialize, Serialize};

/// How the swarm isolates agents' file writes from one another.
///
/// The pre-Isolation runner always created one git worktree per agent
/// when the working directory was a git repo, and fell back to the
/// shared root otherwise. That implicit policy is the wrong default
/// for a codebase that wants the "swarm on one branch" workflow: a
/// shared checkout with observation and notice (see `file_touch.rs`)
/// is often the right answer, and it costs no merge step.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Isolation {
    /// Same checkout, same branch, all agents. File touches are
    /// observed and conflicting peers are notified (P1-c); no
    /// worktrees, no merge. This is the jcode-style workflow.
    Shared,
    /// One git worktree per agent, merged back after the run. The
    /// pre-Isolation behaviour, made explicit. Requires a git
    /// working directory; a non-git root is an error, not a silent
    /// fallback to Shared.
    Worktree,
    /// Worktree when the working directory is a git repo, shared
    /// otherwise. The runner's previous implicit policy.
    #[default]
    Auto,
}

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
    /// Isolation policy. Defaults to `auto`: worktrees when the
    /// working directory is a git repo, shared otherwise — the
    /// runner's behavior before this field existed.
    #[serde(default)]
    pub isolation: Isolation,
    /// Reasoning effort for the coordinator — the decompose and merge
    /// calls. Planning a swarm and reconciling its results is the
    /// hard part of a run, so it defaults to the maximum.
    #[serde(default = "default_root_effort")]
    pub root_effort: kod_types::effort::EffortLevel,
    /// Reasoning effort for the workers. Each runs one subtask, which
    /// is narrower than planning; `Medium` is the default and the
    /// token saving over `Max` is real across N agents.
    #[serde(default = "default_worker_effort")]
    pub worker_effort: kod_types::effort::EffortLevel,
}

fn default_root_effort() -> kod_types::effort::EffortLevel {
    kod_types::effort::EffortLevel::Max
}

fn default_worker_effort() -> kod_types::effort::EffortLevel {
    kod_types::effort::EffortLevel::Medium
}

impl Default for SwarmConfig {
    fn default() -> Self {
        Self {
            max_agents: 5,
            merge_results: true,
            agent_timeout_secs: 300,
            agent_retries: 1,
            timeout_secs: 1800,
            isolation: Isolation::default(),
            root_effort: default_root_effort(),
            worker_effort: default_worker_effort(),
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

    #[test]
    fn isolation_defaults_to_auto() {
        // Auto is the runner's pre-Isolation implicit policy: worktrees
        // when the working directory is a git repo, shared otherwise.
        // A regression to `Shared` would silently stop creating
        // worktrees for a user who never asked for that.
        assert_eq!(SwarmConfig::default().isolation, Isolation::Auto);
    }

    #[test]
    fn isolation_round_trips_each_variant() {
        for variant in [Isolation::Shared, Isolation::Worktree, Isolation::Auto] {
            let c = SwarmConfig {
                isolation: variant,
                ..SwarmConfig::default()
            };
            let toml_str = toml::to_string(&c).unwrap();
            let parsed: SwarmConfig = toml::from_str(&toml_str).unwrap();
            assert_eq!(
                parsed.isolation, variant,
                "round trip failed for {variant:?}"
            );
        }
    }

    #[test]
    fn isolation_parses_from_lowercase_string() {
        // `isolation = "worktree"` in the config file. The
        // `rename_all = "lowercase"` on the enum is what makes this
        // work; a change to `snake_case` would reject "worktree".
        let toml_str = "isolation = \"worktree\"\n";
        let parsed: SwarmConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(parsed.isolation, Isolation::Worktree);
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
        let c: SwarmConfig = toml::from_str("agent_timeout_secs = 0\ntimeout_secs = 0").unwrap();
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
            isolation: Isolation::default(),
            root_effort: default_root_effort(),
            worker_effort: default_worker_effort(),
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

#[cfg(test)]
mod swarm_config_proptests {
    //! **Serde round-trip (oracle) test.** A config type deriving
    //! `Serialize + Deserialize` is a fixed point through TOML.
    //! `SwarmConfig` carries a container-level `#[serde(default)]` and
    //! two field-level `#[serde(default = "...")]` attributes; the
    //! existing `full_config_round_trips_through_toml` example checks
    //! only five of the eight fields, so a rename mismatch or a
    //! default-not-reapplied bug in `isolation`, `root_effort`, or
    //! `worker_effort` passes it unnoticed.
    //!
    //! The property runs serialize → parse → re-serialize and asserts
    //! the two serialized forms are byte-identical (the metamorphic
    //! fixed point), plus a field-by-field comparison for the fields
    //! whose types derive `PartialEq` (catching the case where two
    //! fields are swapped and re-serialize back to the same string).
    use super::*;
    use proptest::prelude::*;

    fn arb_isolation() -> impl Strategy<Value = Isolation> {
        prop::sample::select(vec![
            Isolation::Shared,
            Isolation::Worktree,
            Isolation::Auto,
        ])
    }

    proptest! {
        #[test]
        fn prop_toml_round_trip_is_fixed_point(
            max_agents in 0usize..100,
            merge_results in any::<bool>(),
            agent_timeout_secs in 0u64..10_000,
            agent_retries in 0u32..10,
            timeout_secs in 0u64..100_000,
            isolation in arb_isolation(),
        ) {
            let c = SwarmConfig {
                max_agents,
                merge_results,
                agent_timeout_secs,
                agent_retries,
                timeout_secs,
                isolation,
                root_effort: default_root_effort(),
                worker_effort: default_worker_effort(),
            };

            let toml1 = toml::to_string(&c).expect("serialize");
            let parsed: SwarmConfig = toml::from_str(&toml1).expect("deserialize");

            // Field-by-field for the types that derive PartialEq.
            prop_assert_eq!(parsed.max_agents, c.max_agents);
            prop_assert_eq!(parsed.merge_results, c.merge_results);
            prop_assert_eq!(parsed.agent_timeout_secs, c.agent_timeout_secs);
            prop_assert_eq!(parsed.agent_retries, c.agent_retries);
            prop_assert_eq!(parsed.timeout_secs, c.timeout_secs);
            prop_assert_eq!(parsed.isolation, c.isolation);

            // Metamorphic fixed point: re-serialize. Catches the
            // effort fields (whose type may not derive PartialEq), a
            // rename mismatch, and a #[serde(default)] that does not
            // re-apply the same value.
            let toml2 = toml::to_string(&parsed).expect("re-serialize");
            prop_assert_eq!(toml1, toml2);
        }
    }

    /// Every `Isolation` variant round-trips through the lowercase
    /// form the `#[serde(rename_all = "lowercase")]` attribute
    /// produces.
    #[test]
    fn isolation_variants_round_trip() {
        for variant in [Isolation::Shared, Isolation::Worktree, Isolation::Auto] {
            let c = SwarmConfig {
                isolation: variant,
                ..Default::default()
            };
            let toml_str = toml::to_string(&c).expect("serialize");
            let parsed: SwarmConfig = toml::from_str(&toml_str).expect("deserialize");
            assert_eq!(
                parsed.isolation, variant,
                "isolation did not round-trip: {toml_str}",
            );
        }
    }

    /// A partial TOML with only `max_agents` fills every **other**
    /// field with its default. The existing example test checks three
    /// fields; this one checks all eight, including the two effort
    /// fields via their serialized form (so `EffortLevel` does not
    /// need to gain a `PartialEq` derive purely to serve a test).
    #[test]
    fn partial_toml_fills_every_default() {
        let parsed: SwarmConfig = toml::from_str("max_agents = 3").expect("parse");
        let d = SwarmConfig::default();
        assert_eq!(parsed.max_agents, 3);
        assert_eq!(parsed.merge_results, d.merge_results);
        assert_eq!(parsed.agent_timeout_secs, d.agent_timeout_secs);
        assert_eq!(parsed.agent_retries, d.agent_retries);
        assert_eq!(parsed.timeout_secs, d.timeout_secs);
        assert_eq!(parsed.isolation, d.isolation);
        // Effort compared via `toml::Value` so no derive is forced on
        // a production type for the test's sake.
        let p_root = toml::Value::try_from(parsed.root_effort).expect("root serialize");
        let d_root = toml::Value::try_from(d.root_effort).expect("root default serialize");
        assert_eq!(p_root, d_root, "root_effort default not reapplied");
        let p_worker = toml::Value::try_from(parsed.worker_effort).expect("worker serialize");
        let d_worker = toml::Value::try_from(d.worker_effort).expect("worker default serialize");
        assert_eq!(p_worker, d_worker, "worker_effort default not reapplied");
    }
}
