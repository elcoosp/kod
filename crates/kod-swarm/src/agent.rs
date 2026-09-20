//! Agent definition and lifecycle management.

use kod_error::{KodError, Result};
use kod_types::AgentId;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::time::{Duration, Instant};
use tokio::sync::watch;

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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentState {
    #[default]
    Idle,
    Starting,
    Running,
    Paused,
    Stopping,
    Stopped,
    Failed,
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
            provider: "openai-compatible".to_string(),
            temperature: 0.7,
            max_tokens: 2048,
        }
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
    #[allow(dead_code)]
    model: String,
    max_context_tokens: usize,
    model_config: ModelConfig,
}

impl Agent {
    /// Create a new agent with the given name
    #[allow(clippy::new_ret_no_self)]
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

    /// Get model name.
    ///
    /// Returns the resolved name, which is what `AgentBuilder::with_model`
    /// overrides. The previous implementation read
    /// `model_config.model_name`, i.e. the builder's ModelConfig
    /// default, so `Agent::new("x").with_model("qwen").build().model()`
    /// returned the *default* ("codellama:13b") instead of "qwen" —
    /// the override was stored on the `model` field but never read.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// The full model configuration this agent was built with —
    /// provider, temperature, max_tokens, and the config-file model
    /// name. `model()` returns the resolved name (which
    /// `with_model` can override); this accessor exposes the rest.
    /// Used by status panels and by callers that need to clone an
    /// agent's settings.
    pub fn model_config(&self) -> &ModelConfig {
        &self.model_config
    }

    /// Get max context tokens
    pub fn max_context_tokens(&self) -> usize {
        self.max_context_tokens
    }

    /// Start the agent
    pub async fn start(&self) -> Result<()> {
        if self.state() != AgentState::Idle && self.state() != AgentState::Stopped {
            return Err(KodError::InvalidState(format!(
                "Cannot start agent in state {:?}",
                self.state()
            )));
        }

        // No work happens between Starting and Running today: the
        // agent has no real initialization step. The previous
        // `tokio::time::sleep(Duration::from_millis(10))` labelled
        // "Simulate initialization" was pure waste — every call paid
        // 10 ms of wall clock and every test that drove an agent
        // through its lifecycle paid it too.
        //
        // If a real initialization step is added later (registering
        // with a coordination service, opening a per-agent socket),
        // put its actual await here. The `Starting` state remains in
        // the enum so a caller that subscribes before calling start
        // can observe the transition; today the transition is
        // instantaneous, which is the honest description of the work.
        self.state
            .send(AgentState::Starting)
            .map_err(|e| KodError::InvalidState(format!("Failed to update state: {:?}", e)))?;

        self.state
            .send(AgentState::Running)
            .map_err(|e| KodError::InvalidState(format!("Failed to update state: {:?}", e)))?;

        self.record_heartbeat();

        Ok(())
    }

    /// Pause the agent
    pub async fn pause(&self) -> Result<()> {
        if self.state() != AgentState::Running {
            return Err(KodError::InvalidState(format!(
                "Cannot pause agent in state {:?}",
                self.state()
            )));
        }

        self.state
            .send(AgentState::Paused)
            .map_err(|e| KodError::InvalidState(format!("Failed to update state: {:?}", e)))?;

        Ok(())
    }

    /// Resume the agent
    pub async fn resume(&self) -> Result<()> {
        if self.state() != AgentState::Paused {
            return Err(KodError::InvalidState(format!(
                "Cannot resume agent in state {:?}",
                self.state()
            )));
        }

        self.state
            .send(AgentState::Running)
            .map_err(|e| KodError::InvalidState(format!("Failed to update state: {:?}", e)))?;

        self.record_heartbeat();

        Ok(())
    }

    /// Stop the agent
    pub async fn stop(&self) -> Result<()> {
        match self.state() {
            AgentState::Running | AgentState::Paused | AgentState::Starting => {
                // Same reasoning as start(): no work happens between
                // Stopping and Stopped today. The previous
                // `tokio::time::sleep(Duration::from_millis(10))` with
                // a "Cleanup" comment was a placeholder for work that
                // does not exist. Add the real await here if a
                // shutdown step is added; today the transition is
                // instantaneous.
                self.state.send(AgentState::Stopping).map_err(|e| {
                    KodError::InvalidState(format!("Failed to update state: {:?}", e))
                })?;

                self.state.send(AgentState::Stopped).map_err(|e| {
                    KodError::InvalidState(format!("Failed to update state: {:?}", e))
                })?;
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
        let model = self
            .model
            .unwrap_or_else(|| self.model_config.model_name.clone());

        Agent {
            id: AgentId::new(),
            name: self.name,
            capabilities: self.capabilities,
            state: state_tx,
            state_receiver: state_rx,
            last_heartbeat: parking_lot::Mutex::new(None),
            model,
            max_context_tokens: self.max_context_tokens,
            model_config: self.model_config,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_agent_lifecycle() {
        let agent = Agent::new("test").build();

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

    /// start() and stop() must not contain artificial delays. They
    /// used to sleep 10 ms each, which added up across a swarm of
    /// agents and made lifecycle tests pay for a wall-clock cost that
    /// did no real work. The bound below is generous (100 ms) so a
    /// busy CI machine does not flake; the assertions fail loudly if
    /// a "Simulate initialization" sleep ever returns.
    #[tokio::test]
    async fn test_start_stop_are_not_sleep_bound() {
        use std::time::{Duration, Instant};

        let agent = Agent::new("no-sleep").build();
        let budget = Duration::from_millis(100);

        let t0 = Instant::now();
        agent.start().await.unwrap();
        let start_elapsed = t0.elapsed();
        assert!(
            start_elapsed < budget,
            "start() took {start_elapsed:?} — expected under {budget:?}"
        );

        let t0 = Instant::now();
        agent.stop().await.unwrap();
        let stop_elapsed = t0.elapsed();
        assert!(
            stop_elapsed < budget,
            "stop() took {stop_elapsed:?} — expected under {budget:?}"
        );

        // State machine is unchanged: Idle -> Running -> Stopped.
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

#[cfg(test)]
mod coverage_agent_builder {
    //! The builder's defaults are what a caller that only sets a
    //! name gets. A regression to a model name, a context window,
    //! or a state default silently shifts the behaviour of every
    //! agent a swarm spawns — the caller sees a plausible agent
    //! with the wrong settings and no error to point at.
    use super::*;

    #[test]
    fn default_model_config_matches_the_shipped_values() {
        let c = ModelConfig::default();
        assert_eq!(c.model_name, "codellama:13b");
        assert_eq!(c.provider, "openai-compatible");
        assert!((c.temperature - 0.7).abs() < 1e-6);
        assert_eq!(c.max_tokens, 2048);
    }

    #[test]
    fn default_agent_state_is_idle() {
        // A freshly-built agent must not be Running — the runner's
        // `start_agent` would then refuse the transition.
        assert_eq!(AgentState::default(), AgentState::Idle);
    }

    #[test]
    fn agent_name_is_set_from_the_builder() {
        let a = Agent::new("planner").build();
        assert_eq!(a.name(), "planner");
    }

    #[test]
    fn model_accessor_returns_the_config_default_when_not_overridden() {
        let a = Agent::new("x").build();
        assert_eq!(a.model(), "codellama:13b");
    }

    #[test]
    fn with_model_overrides_the_resolved_model_only() {
        // The override goes into the `model` field, not into
        // `model_config`. This is the design: the config keeps
        // the file's default so a caller inspecting it sees the
        // user's config, while `model()` reports the resolved
        // choice the agent will use on the wire.
        let a = Agent::new("x").with_model("qwen2.5:7b").build();
        assert_eq!(a.model(), "qwen2.5:7b");
        assert_eq!(a.model_config().model_name, "codellama:13b");
    }

    #[test]
    fn max_context_tokens_defaults_to_8192_and_overrides() {
        let a = Agent::new("x").build();
        assert_eq!(a.max_context_tokens(), 8192);
        let b = Agent::new("x").with_max_context_tokens(16_384).build();
        assert_eq!(b.max_context_tokens(), 16_384);
    }

    #[test]
    fn capabilities_accumulate_and_report_their_order() {
        let a = AgentBuilder::new("x")
            .with_capability(Capability::Coding)
            .with_capability(Capability::Testing)
            .with_capability(Capability::Planning)
            .build();
        let caps = a.capabilities();
        assert_eq!(caps.len(), 3);
        assert!(caps.contains(&Capability::Coding));
        assert!(caps.contains(&Capability::Testing));
        assert!(caps.contains(&Capability::Planning));
        assert!(!caps.contains(&Capability::Refactoring));
    }

    #[test]
    fn duplicate_capability_registration_is_idempotent() {
        // `HashSet` under the hood — the same capability added
        // twice appears once. A regression to a Vec would double
        // it and every caller that counts capabilities would
        // drift.
        let a = AgentBuilder::new("x")
            .with_capability(Capability::Coding)
            .with_capability(Capability::Coding)
            .build();
        assert_eq!(a.capabilities().len(), 1);
    }

    #[test]
    fn capability_as_str_round_trips_through_from_str() {
        // `Capability::as_str` is what the runner sends over the
        // wire; `FromStr` is what the config's `[llm.routing.
        // swarm]` keys use. A divergence — a rename on one side
        // only — would silently disable per-capability routing.
        for c in [
            Capability::Coding,
            Capability::Testing,
            Capability::Documentation,
            Capability::CodeReview,
            Capability::Planning,
            Capability::Research,
            Capability::Debugging,
            Capability::Refactoring,
        ] {
            let s = c.as_str();
            let parsed: Capability = s
                .parse()
                .unwrap_or_else(|e| panic!("capability {c:?} -> {s:?} did not parse back: {e}"));
            assert_eq!(parsed, c);
        }
    }

    #[test]
    fn capability_from_str_rejects_unknown_strings() {
        for bad in ["", "Coding", "code_review", "unknown"] {
            assert!(
                bad.parse::<Capability>().is_err(),
                "unexpectedly accepted {bad:?}",
            );
        }
    }

    #[test]
    fn display_matches_as_str() {
        // The `Display` impl and `as_str` must agree — the
        // runner's error messages use Display, its routing uses
        // as_str; a divergence would leave a message naming a
        // capability the routing table does not recognize.
        for c in [
            Capability::Coding,
            Capability::Testing,
            Capability::Documentation,
            Capability::CodeReview,
            Capability::Planning,
            Capability::Research,
            Capability::Debugging,
            Capability::Refactoring,
        ] {
            assert_eq!(format!("{c}"), c.as_str());
        }
    }

    #[test]
    fn a_freshly_built_agent_has_a_unique_id() {
        // Two agents in a swarm must not collide on identity.
        let a = Agent::new("a").build();
        let b = Agent::new("b").build();
        assert_ne!(a.id(), b.id());
    }

    #[test]
    fn heartbeat_is_none_before_start_and_some_after() {
        let a = Agent::new("x").build();
        assert!(a.last_heartbeat().is_none());
        assert!(a.is_timed_out(std::time::Duration::from_millis(0)));
        a.record_heartbeat();
        assert!(a.last_heartbeat().is_some());
        // A freshly-recorded heartbeat is not "timed out" under a
        // generous window.
        assert!(!a.is_timed_out(std::time::Duration::from_secs(60)));
    }
}
