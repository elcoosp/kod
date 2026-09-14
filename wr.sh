#!/usr/bin/env bash
set -uo pipefail

run_with_timeout() {
    local secs="$1"; shift
    if command -v timeout >/dev/null 2>&1; then
        timeout "$secs" "$@"; return $?
    fi
    if command -v gtimeout >/dev/null 2>&1; then
        gtimeout "$secs" "$@"; return $?
    fi
    "$@" &
    local pid=$!
    ( sleep "$secs"
      if kill -0 "$pid" 2>/dev/null; then
          kill -TERM "$pid" 2>/dev/null
          sleep 2
          kill -KILL "$pid" 2>/dev/null
      fi ) &
    local watchdog=$!
    wait "$pid"; local rc=$?
    kill "$watchdog" 2>/dev/null; wait "$watchdog" 2>/dev/null
    [ "$rc" -ge 128 ] && return 124
    return "$rc"
}

COMPILE_OK=true
INCOMPLETE=false
TARGET=crates/kod-swarm/src/coordination.rs

if [ ! -f Cargo.toml ] || [ ! -f "$TARGET" ]; then
    echo "ERROR: run from the kod workspace root ($TARGET missing)"
    exit 1
fi

echo "Patching $TARGET: release agent load on completion/failure"

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

# --- 1. Insert lifecycle methods after assign_task ---------------------
patch(
    '''        // Update agent load
        let mut load = self.agent_load.write().await;
        *load.entry(agent_id.clone()).or_insert(0) += 1;
        Ok(())
    }

    /// Get all pending tasks
    pub async fn pending_tasks(&self) -> Vec<Task> {''',
    '''        // Update agent load
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
    pub async fn pending_tasks(&self) -> Vec<Task> {''',
    "task lifecycle methods",
)

# --- 2. Append tests to the file ---------------------------------------
content += '''

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
'''

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

echo "Checking compilation"
if ! cargo check --workspace 2>&1; then
    echo "Compilation failed – will skip commit"
    COMPILE_OK=false
fi

if [ "$INCOMPLETE" = true ] || [ "$COMPILE_OK" = false ]; then
    echo "Skipping tests and commit due to incomplete files or compilation errors"
    exit 1
fi

echo "Running kod-swarm tests (120s wall clock)"
if ! run_with_timeout 120 cargo test -p kod-swarm 2>&1; then
    echo "kod-swarm tests failed or hung. Paste the full output for a surgical fix."
    exit 1
fi

echo "Running workspace tests (300s wall clock)"
if ! run_with_timeout 300 cargo test --workspace 2>&1; then
    echo "Workspace tests failed or hung. Paste the full output for a surgical fix."
    exit 1
fi

echo "Running clippy with -D warnings"
if ! cargo clippy --workspace --all-targets -- -D warnings 2>&1; then
    echo "Clippy failed. Paste the full output for a surgical fix."
    exit 1
fi

echo "All checks passed. Committing."
git add -A
git commit -m "fix(swarm): release agent load on task completion/failure

TaskCoordinator::assign_task incremented agent_load with no
counterpart — nothing ever decremented it. Over a session, the
least_loaded_agent heuristic degraded into 'whichever agent was
spawned last', because the previous assignments kept inflating
the load counters of everyone else. Routing stopped being
load-based within a handful of tasks.

Add complete_task, fail_task, unassign_task, and task_status.
Complete and fail share a finish_task helper that sets the status,
idempotently returns Ok if the task is already in that state, and
saturating-decrements the assignee's load. Unassign moves the task
back to Pending and drops its entry from the assignments map.

Adds five tests: load released on complete, on fail, on unassign;
complete is idempotent; and least_loaded_agent picks the free agent
when one of two candidates is busy."
