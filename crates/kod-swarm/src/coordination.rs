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
        let task = tasks
            .get_mut(task_id)
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

    /// Mark a task completed and release its slot in the assignee's load.
    ///
    /// Without this, `agent_load` only ever grew (assign_task incremented
    /// it and nothing decremented it), so `least_loaded_agent` degenerated
    /// into "whichever agent was spawned last" after a handful of task
    /// assignments — the load-based routing stopped being load-based.
    pub async fn complete_task(&self, task_id: &TaskId) -> Result<()> {
        self.finish_task(task_id, TaskStatus::Completed).await
    }

    /// Mark a task failed and release its slot in the assignee's load.
    /// Failures free the agent for the next task, same as completions —
    /// a failed task does not keep the agent "busy" forever.
    pub async fn fail_task(&self, task_id: &TaskId) -> Result<()> {
        self.finish_task(task_id, TaskStatus::Failed).await
    }

    /// Common path for complete/fail. Sets the status, decrements the
    /// assignee's load (saturating at zero), and returns the updated
    /// assignment so callers can see who the task had been assigned to.
    async fn finish_task(&self, task_id: &TaskId, status: TaskStatus) -> Result<()> {
        let assigned_to = {
            let mut tasks = self.tasks.write().await;
            let task = tasks.get_mut(task_id).ok_or_else(|| {
                KodError::InvalidState(format!("Task {} not found", task_id))
            })?;
            if task.status == status {
                // Idempotent: completing a completed task is a no-op, not
                // an error. Callers that race on finish should not have to
                // unwind one of them.
                return Ok(());
            }
            task.status = status;
            task.assigned_to.clone()
        };

        if let Some(agent_id) = assigned_to {
            let mut load = self.agent_load.write().await;
            if let Some(slot) = load.get_mut(&agent_id) {
                *slot = slot.saturating_sub(1);
            }
        }
        Ok(())
    }

    /// Unassign a pending task without marking it complete or failed —
    /// useful when a re-plan moves the work to a different agent.
    /// Releases the load slot for the previous assignee.
    pub async fn unassign_task(&self, task_id: &TaskId) -> Result<()> {
        let assigned_to = {
            let mut tasks = self.tasks.write().await;
            let task = tasks.get_mut(task_id).ok_or_else(|| {
                KodError::InvalidState(format!("Task {} not found", task_id))
            })?;
            let prev = task.assigned_to.take();
            task.status = TaskStatus::Pending;
            prev
        };
        if let Some(agent_id) = assigned_to {
            let mut load = self.agent_load.write().await;
            if let Some(slot) = load.get_mut(&agent_id) {
                *slot = slot.saturating_sub(1);
            }
        }
        let mut assignments = self.assignments.write().await;
        assignments.remove(task_id);
        Ok(())
    }

    /// Snapshot of pending + in-progress tasks and their assignees —
    /// useful for a status panel.
    pub async fn task_status(&self, task_id: &TaskId) -> Option<TaskStatus> {
        self.tasks.read().await.get(task_id).map(|t| t.status)
    }

    /// Get all pending tasks
    pub async fn pending_tasks(&self) -> Vec<Task> {
        self.tasks
            .read()
            .await
            .values()
            .filter(|t| t.status == TaskStatus::Pending)
            .cloned()
            .collect()
    }

    /// Get tasks assigned to an agent
    pub async fn tasks_for_agent(&self, agent_id: &AgentId) -> Vec<Task> {
        self.tasks
            .read()
            .await
            .values()
            .filter(|t| t.assigned_to.as_ref() == Some(agent_id))
            .cloned()
            .collect()
    }

    /// Get agent load (number of tasks assigned)
    pub async fn agent_load(&self, agent_id: &AgentId) -> usize {
        self.agent_load
            .read()
            .await
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
        self.assignments.read().await.values().cloned().collect()
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use kod_types::{AgentId, Priority};

    fn task(desc: &str) -> Task {
        Task::new(desc.to_string(), Priority::Medium)
    }

    /// Regression: assign_task used to increment agent_load with no
    /// counterpart, so a coordinator that assigned N tasks to one agent
    /// and completed all of them still reported that agent as loaded.
    /// least_loaded_agent then always picked whichever agent had been
    /// idle the longest — routing stopped being load-based.
    #[tokio::test]
    async fn load_is_released_on_completion() {
        let coord = TaskCoordinator::new();
        let agent = AgentId::new();
        let t1 = task("first");
        let t2 = task("second");
        let t1_id = t1.id.clone();
        let t2_id = t2.id.clone();

        coord.register_task(t1).await.unwrap();
        coord.register_task(t2).await.unwrap();
        coord.assign_task(&t1_id, &agent).await.unwrap();
        coord.assign_task(&t2_id, &agent).await.unwrap();
        assert_eq!(coord.agent_load(&agent).await, 2);

        coord.complete_task(&t1_id).await.unwrap();
        assert_eq!(coord.agent_load(&agent).await, 1);
        coord.complete_task(&t2_id).await.unwrap();
        assert_eq!(coord.agent_load(&agent).await, 0);
    }

    #[tokio::test]
    async fn load_is_released_on_failure() {
        let coord = TaskCoordinator::new();
        let agent = AgentId::new();
        let t = task("will fail");
        let t_id = t.id.clone();

        coord.register_task(t).await.unwrap();
        coord.assign_task(&t_id, &agent).await.unwrap();
        coord.fail_task(&t_id).await.unwrap();
        assert_eq!(coord.agent_load(&agent).await, 0);
        assert_eq!(coord.task_status(&t_id).await, Some(TaskStatus::Failed));
    }

    #[tokio::test]
    async fn load_is_released_on_unassign() {
        let coord = TaskCoordinator::new();
        let agent = AgentId::new();
        let t = task("re-plan");
        let t_id = t.id.clone();

        coord.register_task(t).await.unwrap();
        coord.assign_task(&t_id, &agent).await.unwrap();
        coord.unassign_task(&t_id).await.unwrap();
        assert_eq!(coord.agent_load(&agent).await, 0);
        assert_eq!(coord.task_status(&t_id).await, Some(TaskStatus::Pending));
        assert!(coord.all_assignments().await.is_empty());
    }

    /// Completing an already-completed task is a no-op, not an error —
    /// racing callers should not have to unwind one of themselves.
    #[tokio::test]
    async fn complete_is_idempotent() {
        let coord = TaskCoordinator::new();
        let agent = AgentId::new();
        let t = task("once");
        let t_id = t.id.clone();
        coord.register_task(t).await.unwrap();
        coord.assign_task(&t_id, &agent).await.unwrap();

        coord.complete_task(&t_id).await.unwrap();
        coord.complete_task(&t_id).await.unwrap();
        // Second complete must not underflow the load counter.
        assert_eq!(coord.agent_load(&agent).await, 0);
    }

    #[tokio::test]
    async fn least_loaded_picks_the_free_agent() {
        let coord = TaskCoordinator::new();
        let busy = AgentId::new();
        let free = AgentId::new();

        let t = task("busy work");
        let t_id = t.id.clone();
        coord.register_task(t).await.unwrap();
        coord.assign_task(&t_id, &busy).await.unwrap();

        let candidates = [busy.clone(), free.clone()];
        let picked = coord.least_loaded_agent(&candidates).await.unwrap();
        assert_eq!(picked, free);
    }
}
