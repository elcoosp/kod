#!/usr/bin/env bash
set -uo pipefail

COMPILE_OK=true
TARGET=crates/kod-swarm/src/coordination.rs

if [ ! -f Cargo.toml ] || [ ! -f "$TARGET" ]; then
    echo "ERROR: run from the kod workspace root ($TARGET missing)"
    exit 1
fi

echo "Patching $TARGET: TaskFinish enum + terminal-state guard"

python3 - "$TARGET" << 'PYEOF'
import os
import sys

target = sys.argv[1]
with open(target, "r") as f:
    content = f.read()

def patch(old, new, label, expect=1):
    global content
    n = content.count(old)
    if n == 0:
        print(f"ERROR: old snippet not found: {label}")
        sys.exit(2)
    if expect and n != expect:
        print(f"ERROR: expected {expect} occurrence(s) of {label}, found {n}")
        sys.exit(2)
    content = content.replace(old, new, expect if expect else n)
    print(f"Patched: {label}")

# ----------------------------------------------------------------------
# 1. Introduce TaskFinish and rewrite complete/fail/finish_task.
# ----------------------------------------------------------------------
patch(
    '''    /// Mark a task completed and release its slot in the assignee's load.
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
    }''',
    '''    /// Mark a task completed and release its slot in the assignee's load.
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
            let task = tasks.get_mut(task_id).ok_or_else(|| {
                KodError::InvalidState(format!("Task {} not found", task_id))
            })?;
            let from = task.status;
            if from == status {
                return Ok(TaskFinish::AlreadyInState);
            }
            if matches!(from, TaskStatus::Completed | TaskStatus::Failed) {
                return Err(KodError::InvalidState(format!(
                    "Task {} is already {:?}; refusing to transition to {:?}. \\
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
    }''',
    "TaskFinish enum + finish_task rewrite",
)

# ----------------------------------------------------------------------
# 2. Declare the enum near the top of the file, after TaskAssignment.
# ----------------------------------------------------------------------
patch(
    '''/// Assignment of a task to an agent
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskAssignment {
    pub task_id: TaskId,
    pub agent_id: AgentId,
    pub assigned_at: chrono::DateTime<chrono::Utc>,
    pub capabilities_required: Vec<crate::agent::Capability>,
}''',
    '''/// Assignment of a task to an agent
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
}''',
    "TaskFinish enum declaration",
)

# ----------------------------------------------------------------------
# 3. Extend the existing tests: the return value and the terminal guard.
# ----------------------------------------------------------------------
patch(
    '''    /// Completing an already-completed task is a no-op, not an error —
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
    }''',
    '''    /// Completing an already-completed task is a no-op, not an error —
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
        assert_eq!(
            coord.task_status(&t_id).await,
            Some(TaskStatus::Completed)
        );
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
    }''',
    "TaskFinish tests + terminal guard test",
)

tmp = target + ".tmp"
with open(tmp, "w") as f:
    f.write(content)
os.replace(tmp, target)
print("Wrote", target)
PYEOF

if [ $? -ne 0 ]; then
    echo "ERROR: patching failed"
    exit 1
fi

echo "cargo check --workspace --all-targets"
if ! cargo check --workspace --all-targets 2>&1; then
    echo "Compilation failed"
    exit 1
fi

echo "cargo clippy --workspace --all-targets -- -D warnings"
if ! cargo clippy --workspace --all-targets -- -D warnings 2>&1; then
    echo "Clippy failed"
    exit 1
fi

echo "Committing."
git add -A
git commit -F - <<'MSG'
fix(swarm): report task finish outcome; refuse terminal-to-terminal

TaskCoordinator::complete_task and fail_task both returned Result<()>
and both paths — the actual transition and the idempotent no-op —
returned Ok(()). A caller that raced another finisher had no way to
find out whether it was the one that changed state (and released the
assignee's load slot) or the one that lost.

Add TaskFinish { Transitioned { from: TaskStatus }, AlreadyInState }.
complete_task / fail_task now return Result<TaskFinish>; the enum is
exported alongside the coordinator so a status panel or a
duplicate-detection layer can read it. Existing callers that only
unwrap continue to compile.

The same change fixes a real accounting bug. Before the terminal
guard, fail_task on a Completed task transitioned Completed → Failed
and ran the load decrement a second time — but the load had already
been released by the complete call. The counter that
least_loaded_agent reads is now guarded: finish_task refuses any
transition between two terminal states with an error naming the
statuses and suggesting a new task if the work needs redoing.

Terminal-to-terminal transitions cannot move the counter now, and a
Pending task completed without ever having an assignee is handled
correctly — the transition succeeds, the load slot no-ops.

Extends three existing tests and adds one:
- complete_is_idempotent now asserts Transitioned { from: InProgress }
  then AlreadyInState, and load stays at 0 through both.
- terminal_to_terminal_is_rejected pins the error message and the
  unchanged status.
- complete_unassigned_task_succeeds covers the no-assignee path.
MSG
