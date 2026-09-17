//! Agent swarm orchestrator for managing multiple agents.

use kod_error::{KodError, Result};
use kod_types::AgentId;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::agent::{Agent, Capability};
use crate::communication::AgentCommunicationHub;
use crate::coordination::TaskCoordinator;

/// The agent swarm orchestrator
pub struct AgentSwarm {
    agents: Arc<RwLock<HashMap<AgentId, std::sync::Arc<Agent>>>>,
    communication: AgentCommunicationHub,
    coordinator: TaskCoordinator,
}

impl AgentSwarm {
    /// Create a swarm that owns its own `AgentCommunicationHub`.
    /// Kept for callers (a bench, a test) that have no engine to
    /// share a hub with; the `workspace_root` argument is retained
    /// for API compatibility and is otherwise unused.
    pub fn new(workspace_root: std::path::PathBuf) -> Self {
        let _ = workspace_root;
        Self::with_hub(Arc::new(AgentCommunicationHub::new()))
    }

    /// Create a swarm that shares `hub` with the caller. The
    /// note/read tools (D4.3) live on the same hub, so a swarm
    /// spawned with this constructor has a blackboard visible to
    /// every tool and every agent.
    ///
    /// Cloning the inner `AgentCommunicationHub` (which is itself
    /// two `Arc`-wrapped maps) gives both callers a handle to the
    /// same state — the runner registers its agents on this hub,
    /// and the tools the engine holds read from it.
    pub fn with_hub(hub: Arc<AgentCommunicationHub>) -> Self {
        Self {
            agents: Arc::new(RwLock::new(HashMap::new())),
            communication: (*hub).clone(),
            coordinator: TaskCoordinator::new(),
        }
    }

    /// Add an agent to the swarm. The Agent is wrapped in an Arc so
    /// callers can keep a handle (via [`get_agent`]) and drive its
    /// lifecycle while it stays registered.
    pub async fn add_agent(&self, agent: Agent) -> Result<()> {
        let agent_id = agent.id().clone();
        let agent = std::sync::Arc::new(agent);
        let mut agents = self.agents.write().await;
        if agents.contains_key(&agent_id) {
            return Err(KodError::InvalidState(format!(
                "Agent {} already in swarm",
                agent_id
            )));
        }
        agents.insert(agent_id.clone(), agent);
        drop(agents);
        self.communication.register_agent(agent_id.clone()).await?;
        Ok(())
    }

    /// Remove an agent from the swarm.
    ///
    /// Stops the agent first if it is not already stopped, so anyone
    /// watching its state channel observes the proper
    /// Running → Stopping → Stopped transition instead of a channel
    /// that closes mid-flight. Then unregisters it from the
    /// communication hub so peers stop addressing it.
    pub async fn remove_agent(&self, agent_id: &AgentId) -> Result<()> {
        // Take the agent out under the write lock, but do the stop
        // outside so a slow stop does not hold the swarm's map.
        let agent = {
            let mut agents = self.agents.write().await;
            match agents.remove(agent_id) {
                Some(a) => a,
                None => {
                    return Err(KodError::InvalidState(format!(
                        "Agent {} not in swarm",
                        agent_id
                    )));
                }
            }
        };
        // Best-effort graceful stop: an agent that is already stopped
        // returns Ok; one that is Failed returns an error we do not
        // want to surface as a removal failure. The important
        // invariant is that the agent leaves the swarm.
        if let Err(e) = agent.stop().await {
            tracing::warn!(
                agent = %agent_id,
                error = %e,
                "stop() during remove_agent did not complete cleanly; \
                 removing anyway"
            );
        }
        self.communication.unregister_agent(agent_id).await;
        Ok(())
    }

    /// Stop every agent and clear the swarm.
    ///
    /// Called during graceful shutdown so a session does not leak
    /// agents whose state machines were left mid-flight. Best-effort:
    /// an agent whose stop fails is still removed from the swarm and
    /// the failure is logged; callers get `Ok(())` as long as the
    /// swarm ends empty, because a shutdown that refuses to finish
    /// because one agent misbehaved is worse than a shutdown that
    /// reports the problem and continues.
    pub async fn shutdown(&self) -> Result<()> {
        // Swap the map out so we do not hold the write lock across
        // each stop's await.
        let agents: Vec<(AgentId, std::sync::Arc<Agent>)> = {
            let mut map = self.agents.write().await;
            map.drain().collect()
        };
        let n = agents.len();
        for (id, agent) in &agents {
            if let Err(e) = agent.stop().await {
                tracing::warn!(
                    agent = %id,
                    error = %e,
                    "agent stop failed during swarm shutdown"
                );
            }
            self.communication.unregister_agent(id).await;
        }
        tracing::info!(count = n, "agent swarm shut down");
        Ok(())
    }

    /// Look up an agent by ID.
    ///
    /// Returns the Arc so callers can read its state or drive its
    /// lifecycle. The previous signature returned `Option<AgentId>` —
    /// i.e. echoed the input — which made the agent's actual state
    /// unreachable through the swarm and left the swarm unable to
    /// start, pause, or stop its members.
    pub async fn get_agent(&self, agent_id: &AgentId) -> Option<std::sync::Arc<Agent>> {
        self.agents.read().await.get(agent_id).cloned()
    }

    /// Start the agent with the given ID, if it is present.
    pub async fn start_agent(&self, agent_id: &AgentId) -> Result<()> {
        let agent = self.get_agent(agent_id).await.ok_or_else(|| {
            KodError::InvalidState(format!("Agent {} not in swarm", agent_id))
        })?;
        agent.start().await
    }

    /// Pause the agent with the given ID, if it is present.
    pub async fn pause_agent(&self, agent_id: &AgentId) -> Result<()> {
        let agent = self.get_agent(agent_id).await.ok_or_else(|| {
            KodError::InvalidState(format!("Agent {} not in swarm", agent_id))
        })?;
        agent.pause().await
    }

    /// Resume the agent with the given ID, if it is present.
    pub async fn resume_agent(&self, agent_id: &AgentId) -> Result<()> {
        let agent = self.get_agent(agent_id).await.ok_or_else(|| {
            KodError::InvalidState(format!("Agent {} not in swarm", agent_id))
        })?;
        agent.resume().await
    }

    /// Stop the agent with the given ID, if it is present.
    pub async fn stop_agent(&self, agent_id: &AgentId) -> Result<()> {
        let agent = self.get_agent(agent_id).await.ok_or_else(|| {
            KodError::InvalidState(format!("Agent {} not in swarm", agent_id))
        })?;
        agent.stop().await
    }

    /// List all agents
    pub async fn list_agents(&self) -> Vec<AgentId> {
        self.agents.read().await.keys().cloned().collect()
    }

    /// Find agents with a specific capability.
    ///
    /// Results are sorted by agent name so a caller selecting "the first
    /// agent with capability X" gets the same answer across calls. The
    /// previous implementation collected directly from the underlying
    /// `HashMap`, so the order varied between runs (HashMap iteration
    /// order is unspecified and depends on insertion history and the
    /// random state seed). Anything that leaned on "first" — a
    /// dispatcher picking an agent, a status panel rendering a list,
    /// a test asserting a specific agent was chosen — was
    /// nondeterministic.
    ///
    /// Sorts on name, not id: names are stable and human-meaningful,
    /// and two agents with the same name cannot coexist in a swarm.
    pub async fn find_agents_with_capability(&self, capability: Capability) -> Vec<AgentId> {
        let agents = self.agents.read().await;
        let mut hits: Vec<(String, AgentId)> = agents
            .values()
            .filter(|a| a.has_capability(&capability))
            .map(|a| (a.name().to_string(), a.id().clone()))
            .collect();
        hits.sort_by(|a, b| a.0.cmp(&b.0));
        hits.into_iter().map(|(_, id)| id).collect()
    }

    /// Convenience: does this swarm contain the given agent?
    pub async fn contains_agent(&self, agent_id: &AgentId) -> bool {
        self.agents.read().await.contains_key(agent_id)
    }

    /// Every message a specific agent has participated in (sent,
    /// received, or broadcast), newest-last. Thin wrapper over the
    /// hub's `get_agent_history` so a caller does not need to hold the
    /// hub handle to read it.
    pub async fn agent_messages(
        &self,
        agent_id: &AgentId,
    ) -> Vec<crate::communication::SwarmMessage> {
        self.communication.get_agent_history(agent_id).await
    }

    /// Get the communication hub
    pub fn communication(&self) -> &AgentCommunicationHub {
        &self.communication
    }

    /// Get the task coordinator
    pub fn coordinator(&self) -> &TaskCoordinator {
        &self.coordinator
    }

}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{AgentState, Capability};

    fn swarm_root() -> std::path::PathBuf {
        // TempDir would need a dev-dep; the swarm only uses the path
        // for the workspace's lock map (no filesystem access on the
        // paths exercised here), so a unique non-existent path is fine.
        std::env::temp_dir().join(format!("kod-swarm-test-{}", uuid::Uuid::new_v4()))
    }

    #[tokio::test]
    async fn add_and_get_agent_returns_a_live_handle() {
        let swarm = AgentSwarm::new(swarm_root());
        let agent = Agent::new("solo").with_capability(Capability::Coding).build();
        let id = agent.id().clone();

        swarm.add_agent(agent).await.unwrap();
        let handle = swarm.get_agent(&id).await.expect("agent should be found");
        assert_eq!(handle.name(), "solo");
        assert!(handle.has_capability(&Capability::Coding));
        assert_eq!(handle.state(), AgentState::Idle);
    }

    #[tokio::test]
    async fn swarm_can_drive_agent_lifecycle() {
        let swarm = AgentSwarm::new(swarm_root());
        let agent = Agent::new("worker").build();
        let id = agent.id().clone();
        swarm.add_agent(agent).await.unwrap();

        swarm.start_agent(&id).await.unwrap();
        assert_eq!(swarm.get_agent(&id).await.unwrap().state(), AgentState::Running);

        swarm.pause_agent(&id).await.unwrap();
        assert_eq!(swarm.get_agent(&id).await.unwrap().state(), AgentState::Paused);

        swarm.resume_agent(&id).await.unwrap();
        assert_eq!(swarm.get_agent(&id).await.unwrap().state(), AgentState::Running);

        swarm.stop_agent(&id).await.unwrap();
        assert_eq!(swarm.get_agent(&id).await.unwrap().state(), AgentState::Stopped);
    }

    #[tokio::test]
    async fn get_agent_returns_none_for_unknown_id() {
        let swarm = AgentSwarm::new(swarm_root());
        let unknown = AgentId::new();
        assert!(swarm.get_agent(&unknown).await.is_none());
        assert!(!swarm.contains_agent(&unknown).await);
    }

    #[tokio::test]
    async fn lifecycle_methods_error_for_unknown_id() {
        let swarm = AgentSwarm::new(swarm_root());
        let unknown = AgentId::new();
        assert!(swarm.start_agent(&unknown).await.is_err());
        assert!(swarm.stop_agent(&unknown).await.is_err());
    }

    /// remove_agent on a running agent must leave the agent in the
    /// Stopped state (so anyone holding a state watcher sees a clean
    /// transition) and must remove it from the swarm.
    #[tokio::test]
    async fn remove_running_agent_stops_it_first() {
        let swarm = AgentSwarm::new(swarm_root());
        let agent = Agent::new("leaving").build();
        let id = agent.id().clone();
        swarm.add_agent(agent).await.unwrap();

        // Take a handle and a state watcher before removal so we can
        // observe the pre-removal state and the state transition.
        let handle = swarm.get_agent(&id).await.unwrap();
        let mut watcher = handle.watch_state();

        swarm.start_agent(&id).await.unwrap();
        assert_eq!(handle.state(), AgentState::Running);

        swarm.remove_agent(&id).await.unwrap();
        assert!(!swarm.contains_agent(&id).await);
        assert_eq!(
            handle.state(),
            AgentState::Stopped,
            "removed agent should be Stopped, not left Running"
        );

        // The watcher sees a stop transition rather than a closed
        // channel. `wait_for` returns the new value the first time the
        // predicate matches — a Stopped value was set by stop().
        watcher
            .wait_for(|s| *s == AgentState::Stopped)
            .await
            .expect("state watcher should observe Stopped before the channel closes");
    }

    /// remove_agent on an idle (never-started) agent still removes it
    /// cleanly — stop() on Idle is an error per Agent's own state
    /// machine, and removal must not propagate that as a failure.
    #[tokio::test]
    async fn remove_idle_agent_is_best_effort() {
        let swarm = AgentSwarm::new(swarm_root());
        let agent = Agent::new("idle").build();
        let id = agent.id().clone();
        swarm.add_agent(agent).await.unwrap();

        swarm.remove_agent(&id).await.unwrap();
        assert!(!swarm.contains_agent(&id).await);
    }

    /// shutdown stops every agent in the swarm and empties it.
    #[tokio::test]
    async fn shutdown_stops_all_agents() {
        let swarm = AgentSwarm::new(swarm_root());
        let a = Agent::new("a").build();
        let b = Agent::new("b").build();
        let a_id = a.id().clone();
        let b_id = b.id().clone();
        swarm.add_agent(a).await.unwrap();
        swarm.add_agent(b).await.unwrap();

        let a_handle = swarm.get_agent(&a_id).await.unwrap();
        let b_handle = swarm.get_agent(&b_id).await.unwrap();
        swarm.start_agent(&a_id).await.unwrap();
        swarm.start_agent(&b_id).await.unwrap();

        swarm.shutdown().await.unwrap();

        assert!(swarm.list_agents().await.is_empty(), "swarm should be empty");
        assert_eq!(a_handle.state(), AgentState::Stopped);
        assert_eq!(b_handle.state(), AgentState::Stopped);
    }

    /// `agent_messages` reflects the hub's history for one agent:
    /// a lifecycle broadcast from another agent shows up with the
    /// sender's id.
    #[tokio::test]
    async fn agent_messages_round_trips_lifecycle() {
        let swarm = AgentSwarm::new(swarm_root());
        let a = Agent::new("a").build();
        let b = Agent::new("b").build();
        let a_id = a.id().clone();
        let b_id = b.id().clone();
        swarm.add_agent(a).await.unwrap();
        swarm.add_agent(b).await.unwrap();

        // Take b's receiver so its channel is live.
        let _rx_b = swarm.communication().get_agent_receiver(&b_id).await.unwrap();

        swarm
            .communication()
            .broadcast_lifecycle(&a_id, "started")
            .await
            .unwrap();

        let seen = swarm.agent_messages(&b_id).await;
        assert_eq!(seen.len(), 1, "b should see one message");
        assert_eq!(seen[0].from, a_id);
    }

    /// shutdown on an empty swarm is a no-op, not an error.
    #[tokio::test]
    async fn shutdown_on_empty_swarm_is_ok() {
        let swarm = AgentSwarm::new(swarm_root());
        swarm.shutdown().await.unwrap();
        assert!(swarm.list_agents().await.is_empty());
    }
}
