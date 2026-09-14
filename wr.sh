#!/usr/bin/env bash
set -uo pipefail

COMPILE_OK=true
AGENT=crates/kod-swarm/src/agent.rs
COORD=crates/kod-swarm/src/coordination.rs

for f in "$AGENT" "$COORD"; do
    if [ ! -f "$f" ]; then
        echo "ERROR: missing $f — run from the kod workspace root"
        exit 1
    fi
done

echo "Fixing Agent::model() (reads wrong field) and assign_task reassignment"

python3 - "$AGENT" "$COORD" << 'PYEOF'
import os
import sys

agent, coord = sys.argv[1], sys.argv[2]

def patch(path, old, new, label, expect=1):
    with open(path, "r") as f:
        content = f.read()
    n = content.count(old)
    if n == 0:
        print(f"ERROR: old snippet not found in {path}: {label}")
        sys.exit(2)
    if expect and n != expect:
        print(f"ERROR: expected {expect} occurrence(s) of {label} in {path}, found {n}")
        sys.exit(2)
    patched = content.replace(old, new, expect if expect else n)
    tmp = path + ".tmp"
    with open(tmp, "w") as f:
        f.write(patched)
    os.replace(tmp, path)
    print(f"Patched {path}: {label}")

# ========================================================================
# 1. Agent::model() — returns the wrong field
# ========================================================================
patch(
    agent,
    '''    /// Get model name
    pub fn model(&self) -> &str {
        &self.model_config.model_name
    }''',
    '''    /// Get model name.
    ///
    /// Returns the resolved name, which is what `AgentBuilder::with_model`
    /// overrides. The previous implementation read
    /// `model_config.model_name`, i.e. the builder's ModelConfig
    /// default, so `Agent::new("x").with_model("qwen").build().model()`
    /// returned the *default* ("codellama:13b") instead of "qwen" —
    /// the override was stored on the `model` field but never read.
    pub fn model(&self) -> &str {
        &self.model
    }''',
    "Agent::model() reads resolved name",
)

# ========================================================================
# 2. TaskCoordinator::assign_task — reject reassignment
# ========================================================================
patch(
    coord,
    '''    /// Assign a task to an agent
    pub async fn assign_task(&self, task_id: &TaskId, agent_id: &AgentId) -> Result<()> {
        let mut tasks = self.tasks.write().await;
        let task = tasks
            .get_mut(task_id)
            .ok_or_else(|| KodError::InvalidState(format!("Task {} not found", task_id)))?;
        task.assigned_to = Some(agent_id.clone());
        task.status = TaskStatus::InProgress;
        drop(tasks);''',
    '''    /// Assign a task to an agent.
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
                    "Task {} cannot be assigned: status is {:?}. \\
                     Unassign first to move in-progress work, or use a new task.",
                    task_id, other
                )));
            }
        }
        task.assigned_to = Some(agent_id.clone());
        task.status = TaskStatus::InProgress;
        drop(tasks);''',
    "assign_task rejects non-Pending tasks",
)

print("All patches applied.")
PYEOF

if [ $? -ne 0 ]; then
    echo "ERROR: patching failed"
    exit 1
fi

echo "cargo check --workspace"
if ! cargo check --workspace 2>&1; then
    echo "Compilation failed"
    exit 1
fi

echo "Committing."
git add -A
git commit -m "fix(swarm): correct Agent::model(); reject assign_task on non-Pending

Two bugs found while reviewing the swarm crate against its own
documented contract.

1. Agent::model() returned &self.model_config.model_name, but
   AgentBuilder::build() stores the resolved name (the with_model
   override if set, otherwise model_config.model_name) on the separate
   `model` field. So Agent::new(\"x\").with_model(\"qwen\").build().model()
   returned \"codellama:13b\" — the builder's ModelConfig default — and
   the override was written but never read anywhere.

   Fix: model() returns &self.model. The two fields now agree: `model`
   is the resolved name (also stored as model_config.model_name when
   no override was set), model_config keeps the full config triple.

2. TaskCoordinator::assign_task overwrote task.assigned_to and
   task.status unconditionally. It could therefore reassign a
   Completed, Failed, InProgress, or Blocked task. Each case leaked
   load: finish_task decrements the *current* assignee's counter on
   completion, so reassigning mid-flight left the original agent's
   counter inflated forever; assigning a finished task added a
   counter nobody would ever decrement.

   Fix: assign_task only proceeds from Pending. In-progress work that
   needs to move goes through unassign_task (which releases the old
   assignee's load and returns the task to Pending), then assign_task
   again."
