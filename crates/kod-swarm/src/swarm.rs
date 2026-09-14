//! Agent swarm orchestrator for managing multiple agents.

use kod_error::{KodError, Result};
use kod_types::AgentId;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::agent::{Agent, Capability};
use crate::communication::AgentCommunicationHub;
use crate::coordination::TaskCoordinator;
use crate::workspace::SharedWorkspace;

/// The agent swarm orchestrator
pub struct AgentSwarm {
    agents: Arc<RwLock<HashMap<AgentId, std::sync::Arc<Agent>>>>,
    communication: AgentCommunicationHub,
    coordinator: TaskCoordinator,
    workspace: SharedWorkspace,
}

impl AgentSwarm {
    pub fn new(workspace_root: std::path::PathBuf) -> Self {
        Self {
            agents: Arc::new(RwLock::new(HashMap::new())),
            communication: AgentCommunicationHub::new(),
            coordinator: TaskCoordinator::new(),
            workspace: SharedWorkspace::new(workspace_root),
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

    /// Remove an agent from the swarm
    pub async fn remove_agent(&self, agent_id: &AgentId) -> Result<()> {
        let mut agents = self.agents.write().await;
        if agents.remove(agent_id).is_none() {
            return Err(KodError::InvalidState(format!(
                "Agent {} not in swarm",
                agent_id
            )));
        }
        drop(agents);
        self.communication.unregister_agent(agent_id).await;
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

    /// Find agents with a specific capability
    pub async fn find_agents_with_capability(&self, capability: Capability) -> Vec<AgentId> {
        self.agents
            .read()
            .await
            .values()
            .filter(|a| a.has_capability(&capability))
            .map(|a| a.id().clone())
            .collect()
    }

    /// Convenience: does this swarm contain the given agent?
    pub async fn contains_agent(&self, agent_id: &AgentId) -> bool {
        self.agents.read().await.contains_key(agent_id)
    }

    /// Get the communication hub
    pub fn communication(&self) -> &AgentCommunicationHub {
        &self.communication
    }

    /// Get the task coordinator
    pub fn coordinator(&self) -> &TaskCoordinator {
        &self.coordinator
    }

    /// Get the shared workspace
    pub fn workspace(&self) -> &SharedWorkspace {
        &self.workspace
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
}
