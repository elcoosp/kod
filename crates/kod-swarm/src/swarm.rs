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
    agents: Arc<RwLock<HashMap<AgentId, Agent>>>,
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

    /// Add an agent to the swarm
    pub async fn add_agent(&self, agent: Agent) -> Result<()> {
        let agent_id = agent.id().clone();
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

    /// Get an agent by ID
    pub async fn get_agent(&self, agent_id: &AgentId) -> Option<AgentId> {
        self.agents.read().await.get(agent_id).map(|_| agent_id.clone())
    }

    /// List all agents
    pub async fn list_agents(&self) -> Vec<AgentId> {
        self.agents.read().await.keys().cloned().collect()
    }

    /// Find agents with a specific capability
    pub async fn find_agents_with_capability(&self, capability: Capability) -> Vec<AgentId> {
        self.agents.read().await
            .values()
            .filter(|a| a.has_capability(&capability))
            .map(|a| a.id().clone())
            .collect()
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
