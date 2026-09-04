//! Task coordination and assignment for the agent swarm.

use kod_error::{KodError, Result};
use kod_types::{AgentId, TaskId};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// A task to be assigned to an agent
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    pub description: String,
    pub priority: kod_types::Priority,
    pub status: TaskStatus,
    pub assigned_to: Option<AgentId>,
    pub dependencies: Vec<TaskId>,
}

use kod_types::TaskStatus;

impl Task {
    pub fn new(description: String, priority: kod_types::Priority) -> Self {
        Self {
            id: TaskId::new(),
            description,
            priority,
            status: TaskStatus::Pending,
            assigned_to: None,
            dependencies: vec![],
        }
    }
}

/// Assignment of a task to an agent
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskAssignment {
    pub task_id: TaskId,
    pub agent_id: AgentId,
    pub assigned_at: chrono::DateTime<chrono::Utc>,
    pub capabilities_required: Vec<crate::agent::Capability>,
}

/// Coordinator for task distribution among agents
#[derive(Clone, Default)]
pub struct TaskCoordinator {
    tasks: Arc<RwLock<BTreeMap<TaskId, Task>>>,
    assignments: Arc<RwLock<BTreeMap<TaskId, TaskAssignment>>>,
    agent_load: Arc<RwLock<BTreeMap<AgentId, usize>>>,
}

impl TaskCoordinator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a new task
    pub async fn register_task(&self, task: Task) -> Result<()> {
        let mut tasks = self.tasks.write().await;
        if tasks.contains_key(&task.id) {
            return Err(KodError::InvalidState(format!(
                "Task {} already registered",
                task.id
            )));
        }
        tasks.insert(task.id.clone(), task);
        Ok(())
    }

    /// Assign a task to an agent
    pub async fn assign_task(&self, task_id: &TaskId, agent_id: &AgentId) -> Result<()> {
        let mut tasks = self.tasks.write().await;
        let task = tasks.get_mut(task_id)
            .ok_or_else(|| KodError::InvalidState(format!("Task {} not found", task_id)))?;
        task.assigned_to = Some(agent_id.clone());
        task.status = TaskStatus::InProgress;
        drop(tasks);

        let assignment = TaskAssignment {
            task_id: task_id.clone(),
            agent_id: agent_id.clone(),
            assigned_at: chrono::Utc::now(),
            capabilities_required: vec![],
        };

        let mut assignments = self.assignments.write().await;
        assignments.insert(task_id.clone(), assignment);

        // Update agent load
        let mut load = self.agent_load.write().await;
        *load.entry(agent_id.clone()).or_insert(0) += 1;
        Ok(())
    }

    /// Get all pending tasks
    pub async fn pending_tasks(&self) -> Vec<Task> {
        self.tasks.read().await
            .values()
            .filter(|t| t.status == TaskStatus::Pending)
            .cloned()
            .collect()
    }

    /// Get tasks assigned to an agent
    pub async fn tasks_for_agent(&self, agent_id: &AgentId) -> Vec<Task> {
        self.tasks.read().await
            .values()
            .filter(|t| t.assigned_to.as_ref() == Some(agent_id))
            .cloned()
            .collect()
    }

    /// Get agent load (number of tasks assigned)
    pub async fn agent_load(&self, agent_id: &AgentId) -> usize {
        self.agent_load.read().await
            .get(agent_id)
            .copied()
            .unwrap_or(0)
    }

    /// Find the least loaded agent from a set
    pub async fn least_loaded_agent(&self, agents: &[AgentId]) -> Option<AgentId> {
        let load = self.agent_load.read().await;
        agents
            .iter()
            .min_by_key(|id| load.get(*id).copied().unwrap_or(0))
            .cloned()
    }

    /// Get all task assignments
    pub async fn all_assignments(&self) -> Vec<TaskAssignment> {
        self.assignments.read().await
            .values()
            .cloned()
            .collect()
    }
}
