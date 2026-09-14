#!/usr/bin/env bash
set -uo pipefail

TARGET=crates/kod-swarm/src/coordination.rs

echo "=== Current unassign_task ==="
awk '/pub async fn unassign_task/,/^    \}$/' "$TARGET" | head -30

echo
echo "=== Current finish_task tail ==="
awk '/async fn finish_task/,/^    \}$/' "$TARGET" | tail -20

echo
echo "Patching $TARGET"

python3 - "$TARGET" << 'PYEOF'
import os
import sys

target = sys.argv[1]
with open(target, "r") as f:
    src = f.read()

def patch(old, new, label, expect=1):
    global src
    n = src.count(old)
    if n == 0:
        print(f"  SKIP (anchor absent): {label}")
        return False
    if expect and n != expect:
        print(f"  ERROR: expected {expect} occurrence(s) of {label}, found {n}")
        sys.exit(2)
    src = src.replace(old, new, expect if expect else n)
    print(f"  patched: {label}")
    return True

# ----------------------------------------------------------------------
# 1. Rewrite unassign_task to refuse terminal states and return a
#    TaskFinish so a caller can tell "unassigned" from "nothing to do".
# ----------------------------------------------------------------------
patch(
    '''    /// Unassign a pending task without marking it complete or failed —
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
    }''',
    '''    /// Unassign a task without marking it complete or failed — useful
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
            let task = tasks.get_mut(task_id).ok_or_else(|| {
                KodError::InvalidState(format!("Task {} not found", task_id))
            })?;
            let from = task.status;
            if matches!(from, TaskStatus::Completed | TaskStatus::Failed) {
                return Err(KodError::InvalidState(format!(
                    "Task {} is already {:?}; refusing to unassign. Create a new task \\
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
    }''',
    "unassign_task refuses terminal, returns TaskFinish",
)

# ----------------------------------------------------------------------
# 2. Tests: extending the existing swarm test block. Idempotent.
# ----------------------------------------------------------------------
if "test_unassign_refuses_terminal_task" not in src:
    anchor = '''    #[tokio::test]
    async fn least_loaded_picks_the_free_agent() {'''
    if anchor not in src:
        print("  ERROR: test anchor not found")
        sys.exit(2)
    new_tests = '''    /// unassign_task must refuse a task that has already reached a
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
        assert_eq!(
            coord.task_status(&t_id).await,
            Some(TaskStatus::Completed)
        );
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
        assert_eq!(
            coord.task_status(&t_id).await,
            Some(TaskStatus::Pending)
        );
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
        assert_eq!(
            coord.task_status(&t_id).await,
            Some(TaskStatus::Pending)
        );
    }

    #[tokio::test]
    async fn least_loaded_picks_the_free_agent() {'''
    src = src.replace(anchor, new_tests, 1)
    print("  added unassign tests")
else:
    print("  unassign tests already present")

tmp = target + ".tmp"
with open(tmp, "w") as f:
    f.write(src)
os.replace(tmp, target)
print("Wrote", target)
PYEOF

if [ $? -ne 0 ]; then
    echo "ERROR: patching failed"
    exit 1
fi

echo
echo "cargo check --workspace --all-targets 2>&1 | tail -15"
if ! cargo check --workspace --all-targets 2>&1 | tail -15; then
    echo "Compilation failed"
    exit 1
fi

cat > /tmp/kod_commit_msg.txt <<'MSG'
fix(swarm): unassign_task refuses terminal tasks, returns TaskFinish

complete_task and fail_task do not clear assigned_to when a task
finishes — the field is left as the historical record of who did
the work. unassign_task took assigned_to unconditionally,
decremented the assignee's load, and reset the status to Pending.
Given a task that had already completed, that meant:

  - the assignee's load was decremented a second time (the first
    decrement was at complete_task), so least_loaded_agent's view
    of that agent drifted below reality;
  - the task re-entered Pending, and a subsequent assign_task would
    hand the finished work to a different agent.

Refuse any unassign on a Completed or Failed task, with an error
naming the state and pointing at "create a new task if the work
needs redoing."

Also change the return type from Result<()> to Result<TaskFinish>,
matching complete_task and fail_task. The three transition methods
now share one convention: Transitioned { from } when state changed,
AlreadyInState when the call was a no-op. The previous Result<()>
made a no-op indistinguishable from a real transition, so a
caller that wanted to log "task moved back to Pending" had no way
to know it should.

Adds three tests: unassign on a Completed task errors and leaves
the load at 0 and the status Completed; unassign on an InProgress
task reports Transitioned { from: InProgress }, releases the slot,
and empties the assignments map; unassign on a Pending task reports
AlreadyInState and changes nothing.
MSG

git add -A
git commit -F /tmp/kod_commit_msg.txt
rm -f /tmp/kod_commit_msg.txt
