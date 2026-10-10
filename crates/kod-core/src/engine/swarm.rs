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

    /// Test-only: how many transcripts are currently subscribed as
    /// blackboard viewers. A leaked subscription is a slow memory
    /// leak on a long-lived engine (the daemon case); the swarm
    /// runner's cleanup loop must clear the viewer for every handle
    /// it created, including subtasks that were failed-out by a
    /// failed dependency and never dispatched.
    pub async fn blackboard_viewer_count_for_tests(&self) -> usize {
        self.blackboard_viewers.read().await.len()
    }

    /// Try to become the engine's one active swarm. Returns a guard
    /// that releases the slot on drop (all return paths, including
    /// `?`), or `None` when a swarm is already running.
    ///
    /// `compare_exchange` is what makes this race-free: two callers
    /// that arrive together see one success and one failure. A
    /// `load` then `store` would let both proceed.
    pub fn try_acquire_swarm_slot(&self) -> Option<SwarmSlotGuard> {
        use std::sync::atomic::Ordering;
        match self
            .swarm_slot
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => Some(SwarmSlotGuard {
                slot: std::sync::Arc::clone(&self.swarm_slot),
            }),
            Err(_) => None,
        }
    }

    /// The swarm blackboard (Tier 3.5).
    /// Install the shared file-touch bus for a swarm run, and return
    /// a `SwarmRunGuard` that clears it on drop.
    ///
    /// Called once by the runner before it spawns agents. The bus
    /// must be removed when the run ends, but "ends" includes
    /// cancellation: the TUI aborts `gen_task` on Esc and `serve`
    /// drops its `run_handle` on client disconnect, and both drop
    /// the run's future without executing the async cleanup at the
    /// bottom of `run`. The returned guard clears the slot from
    /// `Drop` — synchronously — so a cancelled run cannot leave the
    /// engine holding a stale bus whose subscribers are already
    /// gone. Without the guard, the next non-swarm turn on that
    /// engine would install a file-touch hook (see
    /// `run_tool_calls`) that published on a bus nobody reads, and
    /// an unrelated turn would be attributed to a phantom agent.
    ///
    /// Idempotent: calling twice replaces the bus and returns a new
    /// guard. The runner only calls it once per run.
    pub fn install_swarm_file_bus(
        self: &Arc<Self>,
        bus: std::sync::Arc<kod_swarm::file_touch::FileTouchBus>,
        service: std::sync::Arc<kod_swarm::file_touch::FileTouchService>,
    ) -> SwarmRunGuard {
        *self.swarm_file_bus.write() = Some(SwarmFileBus { bus, service });
        // Capture the runtime handle *now*, not at drop time: a
        // cancelled run's future may be dropped on a task that is
        // itself being torn down, and `Handle::current()` inside
        // `Drop` would panic. Spawning the async cleanup on the
        // captured handle schedules it on the live runtime
        // regardless of where the drop happens.
        SwarmRunGuard {
            slot: std::sync::Arc::clone(&self.swarm_file_bus),
            hub: Arc::clone(&self.swarm_hub),
            runtime: tokio::runtime::Handle::current(),
            engine: Arc::clone(self),
            dispatch_keys: std::sync::Arc::new(parking_lot::Mutex::new(Vec::new())),
        }
    }

    /// Remove the file-touch bus. Safe to call when none is installed.
    /// Kept as a manual escape hatch for tests and for the runner's
    /// early-error paths that must release the bus before the guard
    /// is dropped.
    pub fn uninstall_swarm_file_bus(&self) {
        *self.swarm_file_bus.write() = None;
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
    pub fn checkpoints(&self) -> Option<&Arc<kod_core_state::checkpoint::CheckpointManager>> {
        self.checkpoints.as_ref()
    }

    /// The working directory tools are rooted at.
    pub fn working_dir(&self) -> &std::path::Path {
        &self.working_dir
    }
}

/// RAII guard for the engine's exclusive swarm slot. Dropped by
/// every `SwarmRunner::run` return path, including a `?` on an
/// error, so a failed run does not deadlock the next one.
pub struct SwarmSlotGuard {
    slot: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Drop for SwarmSlotGuard {
    fn drop(&mut self) {
        self.slot.store(false, std::sync::atomic::Ordering::Release);
    }
}

impl KodEngine {
    /// Test-only: is a swarm file-touch bus currently installed on
    /// this engine? A cancelled swarm run (its future dropped
    /// mid-flight) must leave this `false`; the pre-fix shape left
    /// the bus behind because the async `uninstall_swarm_file_bus`
    /// call sat after an `await` that a dropped future never
    /// reaches.
    pub fn swarm_file_bus_installed_for_tests(&self) -> bool {
        self.swarm_file_bus.read().is_some()
    }
}

/// RAII guard for the engine's swarm file-touch bus. Cleared on
/// drop, so a cancelled swarm run (a dropped future) releases the
/// slot even when its async cleanup path is never reached.
pub struct SwarmRunGuard {
    slot: std::sync::Arc<parking_lot::RwLock<Option<SwarmFileBus>>>,
    hub: Arc<kod_swarm::AgentCommunicationHub>,
    runtime: tokio::runtime::Handle,
    engine: Arc<KodEngine>,
    dispatch_keys: std::sync::Arc<parking_lot::Mutex<Vec<String>>>,
}

impl SwarmRunGuard {
    /// Register a dispatch's transcript key with this guard, so the
    /// engine forgets it if the run's future is dropped before the
    /// normal end-of-run cleanup runs. Idempotent.
    ///
    /// The runner calls this from the dispatch loop, before the
    /// subtask's agent runs. On a successful run the same keys are
    /// forgotten by the runner's own end-of-run loop, so a guard
    /// that drops on success forgets nothing (already empty maps).
    /// On cancellation the guard's `Drop` is the *only* place the
    /// keys are forgotten, which is why this must be kept in sync
    /// with the runner's normal cleanup.
    pub fn track_dispatch_key(&self, key: &str) {
        self.dispatch_keys.lock().push(key.to_string());
    }
}

impl Drop for SwarmRunGuard {
    fn drop(&mut self) {
        *self.slot.write() = None;
        // The engine's per-transcript maps, the hub's maps, and the
        // bus slot's siblings all sit behind `tokio` `RwLock`s
        // (sync `Drop` cannot `await`), so the cleanup is spawned on
        // the runtime handle captured at installation. The spawned
        // future is independent of the run's future tree, so a
        // cancelled run still releases everything it set up.
        let hub = Arc::clone(&self.hub);
        let engine = Arc::clone(&self.engine);
        let keys: Vec<String> = self.dispatch_keys.lock().clone();
        self.runtime.spawn(async move {
            for key in keys {
                engine.forget_transcript(&key).await;
            }
            hub.clear_all().await;
        });
    }
}
