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

/// Outcome of a `complete_task` / `fail_task` call.
///
/// The distinction matters when two callers race to finish the same
/// task: `Transitioned` is the one that actually changed the state and
/// released the assignee's load slot; `AlreadyInState` is the one that
/// lost the race and correctly did nothing. Before this, both callers
/// saw `Ok(())` and a caller that needed to know which side it was on
/// (to decide whether to report a duplicate completion, or to update a
/// status panel) had no way to find out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskFinish {
    /// The task moved from a non-terminal status to the requested
    /// terminal one. `from` is the previous status. The assignee's load
    /// counter was decremented if the task had an assignee.
    Transitioned { from: TaskStatus },
    /// The task was already in the requested terminal status. No state
    /// changed and no counter moved.
    AlreadyInState,
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

    /// Assign a task to an agent.
    ///
    /// Rejects three cases that the previous implementation allowed
    /// and that silently corrupted the load counters:
    ///
    /// - The task is already Completed or Failed. Assigning finished
    ///   work created a new InProgress assignment and incremented a
    ///   load counter that nothing would decrement for the new agent,
    ///   because finish_task only decrements for the assignee recorded
    ///   at completion time.
    /// - The task is already InProgress. Reassigning mid-flight left
    ///   the original agent's load inflated forever — its slot was
    ///   released only on complete/fail, which no longer matched the
    ///   task it held.
    /// - The task is Blocked. Blocked work must be unblocked (or
    ///   unassigned) before it can be picked up again.
    ///
    /// Callers who want to move in-progress work to a different agent
    /// should `unassign_task` first, which releases the current
    /// assignee's load and returns the task to Pending.
    pub async fn assign_task(&self, task_id: &TaskId, agent_id: &AgentId) -> Result<()> {
        let mut tasks = self.tasks.write().await;
        let task = tasks
            .get_mut(task_id)
            .ok_or_else(|| KodError::InvalidState(format!("Task {} not found", task_id)))?;
        match task.status {
            TaskStatus::Pending => {}
            other => {
                return Err(KodError::InvalidState(format!(
                    "Task {} cannot be assigned: status is {:?}. \
                     Unassign first to move in-progress work, or use a new task.",
                    task_id, other
                )));
            }
        }
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
    ///
    /// The returned [`TaskFinish`] tells the caller whether the call
    /// actually transitioned the task, was a no-op because the task was
    /// already Completed, or — for the terminal-to-terminal case —
    /// errored. The previous `Result<()>` returned `Ok(())` for both the
    /// transition and the no-op, so a caller that wanted to know "did my
    /// completion land, or did someone else get there first?" had no way
    /// to find out.
    pub async fn complete_task(&self, task_id: &TaskId) -> Result<TaskFinish> {
        self.finish_task(task_id, TaskStatus::Completed).await
    }

    /// Mark a task failed and release its slot in the assignee's load.
    /// Failures free the agent for the next task, same as completions —
    /// a failed task does not keep the agent "busy" forever.
    ///
    /// See [`TaskCoordinator::complete_task`] for the return semantics.
    pub async fn fail_task(&self, task_id: &TaskId) -> Result<TaskFinish> {
        self.finish_task(task_id, TaskStatus::Failed).await
    }

    /// Common path for complete/fail.
    ///
    /// On a real transition (from a non-terminal status to the requested
    /// one), decrements the assignee's load (saturating at zero) and
    /// returns [`TaskFinish::Transitioned`] naming the previous status.
    /// On a same-status call, returns [`TaskFinish::AlreadyInState`]
    /// without touching the load — a caller that raced another finisher
    /// sees exactly that.
    ///
    /// Refuses to move from one terminal state to another. Before this
    /// guard, `fail_task` on a Completed task transitioned Completed →
    /// Failed and decremented the load a second time, corrupting the
    /// counter that `least_loaded_agent` reads. Terminal is terminal;
    /// if the work needs redoing, the caller creates a new task.
    async fn finish_task(&self, task_id: &TaskId, status: TaskStatus) -> Result<TaskFinish> {
        let (from, assigned_to) = {
            let mut tasks = self.tasks.write().await;
            let task = tasks
                .get_mut(task_id)
                .ok_or_else(|| KodError::InvalidState(format!("Task {} not found", task_id)))?;
            let from = task.status;
            if from == status {
                return Ok(TaskFinish::AlreadyInState);
            }
            if matches!(from, TaskStatus::Completed | TaskStatus::Failed) {
                return Err(KodError::InvalidState(format!(
                    "Task {} is already {:?}; refusing to transition to {:?}. \
                     Create a new task if the work needs redoing.",
                    task_id, from, status
                )));
            }
            task.status = status;
            (from, task.assigned_to.clone())
        };

        if let Some(agent_id) = &assigned_to {
            let mut load = self.agent_load.write().await;
            if let Some(slot) = load.get_mut(agent_id) {
                *slot = slot.saturating_sub(1);
            }
        }
        Ok(TaskFinish::Transitioned { from })
    }

    /// Unassign a task without marking it complete or failed — useful
    /// when a re-plan moves the work to a different agent. Releases
    /// the load slot for the previous assignee and returns the task to
    /// Pending.
    ///
    /// Returns [`TaskFinish::Transitioned`] when the task moved from
    /// InProgress (or Blocked) to Pending, and
    /// [`TaskFinish::AlreadyInState`] when the task was already
    /// Pending and no state changed — the same shape
    /// [`TaskCoordinator::complete_task`] and [`TaskCoordinator::fail_task`]
    /// use, so a caller does not have to switch conventions between
    /// the three terminal transitions.
    ///
    /// Refuses to unassign a task that has already reached a terminal
    /// state. `complete_task` and `fail_task` do not clear
    /// `assigned_to` when the task finishes (the field is left as the
    /// historical record of who did the work), so the previous
    /// implementation — which unconditionally took `assigned_to` and
    /// decremented the load — would decrement a second time on any
    /// already-finished task and reset its status to Pending. The
    /// load counter drifted negative-by-one and the task appeared to
    /// need doing again. Terminal is terminal; a caller that needs to
    /// redo the work creates a new task.
    pub async fn unassign_task(&self, task_id: &TaskId) -> Result<TaskFinish> {
        let (from, was_assigned) = {
            let mut tasks = self.tasks.write().await;
            let task = tasks
                .get_mut(task_id)
                .ok_or_else(|| KodError::InvalidState(format!("Task {} not found", task_id)))?;
            let from = task.status;
            if matches!(from, TaskStatus::Completed | TaskStatus::Failed) {
                return Err(KodError::InvalidState(format!(
                    "Task {} is already {:?}; refusing to unassign. Create a new task \
                     if the work needs redoing.",
                    task_id, from
                )));
            }
            if from == TaskStatus::Pending {
                return Ok(TaskFinish::AlreadyInState);
            }
            let was_assigned = task.assigned_to.take().is_some();
            task.status = TaskStatus::Pending;
            (from, was_assigned)
        };
        if was_assigned
            && let Some(prev_assignee) = self
                .assignments
                .read()
                .await
                .get(task_id)
                .map(|a| a.agent_id.clone())
        {
            let mut load = self.agent_load.write().await;
            if let Some(slot) = load.get_mut(&prev_assignee) {
                *slot = slot.saturating_sub(1);
            }
        }
        let mut assignments = self.assignments.write().await;
        assignments.remove(task_id);
        Ok(TaskFinish::Transitioned { from })
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
    /// The second call must report `AlreadyInState`, and must not
    /// touch the load counter.
    #[tokio::test]
    async fn complete_is_idempotent() {
        let coord = TaskCoordinator::new();
        let agent = AgentId::new();
        let t = task("once");
        let t_id = t.id.clone();
        coord.register_task(t).await.unwrap();
        coord.assign_task(&t_id, &agent).await.unwrap();

        let first = coord.complete_task(&t_id).await.unwrap();
        assert_eq!(
            first,
            TaskFinish::Transitioned {
                from: TaskStatus::InProgress
            },
            "first complete should transition InProgress -> Completed"
        );
        assert_eq!(coord.agent_load(&agent).await, 0);

        let second = coord.complete_task(&t_id).await.unwrap();
        assert_eq!(
            second,
            TaskFinish::AlreadyInState,
            "second complete should report AlreadyInState"
        );
        // Load stays at 0 — the no-op must not decrement again.
        assert_eq!(coord.agent_load(&agent).await, 0);
    }

    /// A task that reached one terminal state cannot move to the
    /// other. Regression: the previous implementation allowed
    /// `fail_task` on a Completed task, which transitioned Completed →
    /// Failed and decremented the load a second time — corrupting the
    /// counter `least_loaded_agent` reads.
    #[tokio::test]
    async fn terminal_to_terminal_is_rejected() {
        let coord = TaskCoordinator::new();
        let agent = AgentId::new();
        let t = task("done already");
        let t_id = t.id.clone();
        coord.register_task(t).await.unwrap();
        coord.assign_task(&t_id, &agent).await.unwrap();

        coord.complete_task(&t_id).await.unwrap();
        assert_eq!(coord.agent_load(&agent).await, 0);

        let err = coord.fail_task(&t_id).await.unwrap_err();
        match err {
            KodError::InvalidState(msg) => {
                assert!(
                    msg.contains("Completed") && msg.contains("Failed"),
                    "error should name both statuses: {msg}"
                );
            }
            other => panic!("expected InvalidState, got {other:?}"),
        }
        // The rejected transition must not have decremented the load.
        assert_eq!(coord.agent_load(&agent).await, 0);
        // And the task status must be unchanged.
        assert_eq!(coord.task_status(&t_id).await, Some(TaskStatus::Completed));
    }

    /// A Pending task can be completed without ever being assigned.
    /// The load counter must stay at 0 — there is no assignee to
    /// release.
    #[tokio::test]
    async fn complete_unassigned_task_succeeds() {
        let coord = TaskCoordinator::new();
        let t = task("never picked up");
        let t_id = t.id.clone();
        coord.register_task(t).await.unwrap();

        let outcome = coord.complete_task(&t_id).await.unwrap();
        assert_eq!(
            outcome,
            TaskFinish::Transitioned {
                from: TaskStatus::Pending
            }
        );
        assert_eq!(coord.task_status(&t_id).await, Some(TaskStatus::Completed));
    }

    /// unassign_task must refuse a task that has already reached a
    /// terminal state. Regression: the previous implementation took
    /// assigned_to unconditionally, decremented the assignee's load
    /// counter a second time (the first decrement was at
    /// complete_task), and reset the task to Pending — corrupting the
    /// load accounting and resurrecting finished work.
    #[tokio::test]
    async fn test_unassign_refuses_terminal_task() {
        let coord = TaskCoordinator::new();
        let agent = AgentId::new();
        let t = task("done");
        let t_id = t.id.clone();
        coord.register_task(t).await.unwrap();
        coord.assign_task(&t_id, &agent).await.unwrap();

        coord.complete_task(&t_id).await.unwrap();
        assert_eq!(coord.agent_load(&agent).await, 0);

        let err = coord.unassign_task(&t_id).await.unwrap_err();
        match err {
            KodError::InvalidState(msg) => {
                assert!(
                    msg.contains("Completed"),
                    "error should name the status: {msg}"
                );
            }
            other => panic!("expected InvalidState, got {other:?}"),
        }
        // Load stayed at 0 — no second decrement.
        assert_eq!(coord.agent_load(&agent).await, 0);
        // Status stayed Completed — no resurrection.
        assert_eq!(coord.task_status(&t_id).await, Some(TaskStatus::Completed));
    }

    /// unassign_task on an InProgress task returns Transitioned and
    /// releases the load slot.
    #[tokio::test]
    async fn test_unassign_in_progress_releases_load() {
        let coord = TaskCoordinator::new();
        let agent = AgentId::new();
        let t = task("re-plan");
        let t_id = t.id.clone();
        coord.register_task(t).await.unwrap();
        coord.assign_task(&t_id, &agent).await.unwrap();
        assert_eq!(coord.agent_load(&agent).await, 1);

        let outcome = coord.unassign_task(&t_id).await.unwrap();
        assert_eq!(
            outcome,
            TaskFinish::Transitioned {
                from: TaskStatus::InProgress
            }
        );
        assert_eq!(coord.agent_load(&agent).await, 0);
        assert_eq!(coord.task_status(&t_id).await, Some(TaskStatus::Pending));
        assert!(coord.all_assignments().await.is_empty());
    }

    /// unassign_task on a Pending task reports AlreadyInState and
    /// changes nothing. A caller that races a re-plan sees exactly
    /// that, without having to unwind.
    #[tokio::test]
    async fn test_unassign_pending_is_no_op() {
        let coord = TaskCoordinator::new();
        let t = task("untouched");
        let t_id = t.id.clone();
        coord.register_task(t).await.unwrap();

        let outcome = coord.unassign_task(&t_id).await.unwrap();
        assert_eq!(outcome, TaskFinish::AlreadyInState);
        assert_eq!(coord.task_status(&t_id).await, Some(TaskStatus::Pending));
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
