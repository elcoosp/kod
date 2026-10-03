use super::*;

impl KodEngine {
    /// Enable or disable the shared blackboard in a transcript's
    /// prompt (Tier 3.5). The swarm runner calls this before each
    /// agent starts.
    pub async fn set_blackboard_viewer(&self, key: &str, on: bool) {
        let mut g = self.blackboard_viewers.write().await;
        if on {
            g.insert(key.to_string());
        } else {
            g.remove(key);
        }
    }

    /// The swarm blackboard (Tier 3.5).
    /// Install the shared file-touch bus for a swarm run.
    ///
    /// Called once by the runner before it spawns agents. Removing it
    /// (`uninstall_swarm_file_bus`) when the run ends is what keeps a
    /// long-lived engine from observing touches on a later non-swarm
    /// turn.
    pub async fn install_swarm_file_bus(
        &self,
        bus: std::sync::Arc<kod_swarm::file_touch::FileTouchBus>,
        service: std::sync::Arc<kod_swarm::file_touch::FileTouchService>,
    ) {
        *self.swarm_file_bus.write().await = Some(SwarmFileBus { bus, service });
    }

    /// Remove the file-touch bus. Safe to call when none is installed.
    pub async fn uninstall_swarm_file_bus(&self) {
        *self.swarm_file_bus.write().await = None;
    }

    /// Build the per-call file-touch hook for a swarm transcript.
    ///
    /// Split out from `run_tool_calls` so it is testable without an
    /// engine: the returned hook records the touch in the service and
    /// publishes it on the bus, in that order — a subscriber that
    /// sees an event is guaranteed the service already knows about
    /// it, so `conflicts_for` answers correctly on receipt.
    ///
    /// The `holder` argument from the closure (the `ToolContext`'s
    /// own holder string) is used as the agent id, not the captured
    /// value, so a context re-used under a different holder still
    /// attributes correctly.
    pub(crate) fn build_swarm_file_hook(
        bus: std::sync::Arc<kod_swarm::file_touch::FileTouchBus>,
        service: std::sync::Arc<kod_swarm::file_touch::FileTouchService>,
    ) -> kod_tools::context::FileTouchHook {
        kod_tools::context::FileTouchHook::new(move |holder, path, op, intent| {
            let kod_op = match op {
                kod_tools::context::FileOp::Read => kod_swarm::file_touch::FileOp::Read,
                kod_tools::context::FileOp::Write => kod_swarm::file_touch::FileOp::Write,
                kod_tools::context::FileOp::Edit => kod_swarm::file_touch::FileOp::Edit,
            };
            let touch = kod_swarm::file_touch::FileTouch {
                agent_id: holder.to_string(),
                path: path.to_path_buf(),
                op: kod_op,
                summary: None,
                intent: intent.map(str::to_string),
                at: std::time::Instant::now(),
            };
            service.record(touch.clone());
            bus.publish(touch);
        })
    }

    pub fn blackboard(&self) -> &kod_swarm::Blackboard {
        &self.blackboard
    }

    /// Publish a file discovery to the blackboard. Called after a
    /// successful `read_file` / `grep` / `search_files`.
    pub fn note_file_seen(&self, agent: &str, path: &str, summary: &str) {
        self.blackboard.put(
            format!("file:{path}"),
            serde_json::json!({
                "path": path,
                "summary": summary,
            }),
            agent,
            kod_swarm::AuthorKind::Engine,
            vec!["file".to_string(), "team".to_string()],
        );
    }

    /// Publish a write claim. Called by the swarm runner before an
    /// agent runs.
    pub fn note_write_claim(&self, agent: &str, glob: &str) {
        self.blackboard.put(
            format!("claim:{agent}:{glob}"),
            serde_json::json!({ "glob": glob, "agent": agent }),
            agent,
            kod_swarm::AuthorKind::Engine,
            vec!["claim".to_string(), "team".to_string()],
        );
    }

    /// Publish a completed subtask. Called by the swarm runner when
    /// an agent finishes.
    pub fn note_subtask_done(&self, agent: &str, name: &str, summary: &str) {
        self.blackboard.put(
            format!("done:{name}"),
            serde_json::json!({
                "subtask": name,
                "summary": summary,
                "agent": agent,
            }),
            agent,
            kod_swarm::AuthorKind::Engine,
            vec!["done".to_string(), "team".to_string()],
        );
    }

    /// The communication hub the swarm's blackboard lives on.
    /// Cloning the `Arc` gives a caller a handle to the same hub the
    /// note/read tools talk to, and that a `SwarmRunner` uses to
    /// spawn its agents.
    pub fn swarm_hub(&self) -> Arc<kod_swarm::AgentCommunicationHub> {
        Arc::clone(&self.swarm_hub)
    }

    /// The engine's identity on the hub — the sender id the note
    /// tool broadcasts as. Stable across the engine's lifetime.
    pub fn swarm_coordinator_id(&self) -> &kod_types::AgentId {
        &self.swarm_coordinator_id
    }

    /// The engine's shared todo list. A caller that wants to seed it
    /// before a session, or render it alongside the chat, reads this
    /// directly.
    pub fn todo_list(&self) -> &kod_tools::TodoList {
        &self.todo_list
    }

    /// The engine's shared per-path lock table. A caller that wants
    /// to hold a lock itself (a test, an embedder coordinating with an
    /// agent) acquires from this table directly.
    pub fn path_lock_table(&self) -> Arc<PathLockTable> {
        Arc::clone(&self.lock_table)
    }

    /// The engine's checkpoint manager, when a checkpoint directory
    /// could be determined. `None` means the engine cannot snapshot
    /// (no home directory); a caller that offers `/rollback` should
    /// say so rather than silently no-op.
    pub fn checkpoints(&self) -> Option<&Arc<crate::checkpoint::CheckpointManager>> {
        self.checkpoints.as_ref()
    }

    /// The working directory tools are rooted at.
    pub fn working_dir(&self) -> &std::path::Path {
        &self.working_dir
    }
}
