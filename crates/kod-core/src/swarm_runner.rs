//! Multi-agent dispatch: decompose a goal, run one agent per subtask
//! concurrently, merge the results.
//!
//! The `kod-swarm` crate provides the coordination primitives — agents
//! with lifecycles and capabilities, a communication hub, a task
//! coordinator that tracks per-agent load, a shared workspace with file
//! locks. This module is the piece that was missing: it wires those
//! primitives to the engine's agentic loop, so `kod swarm -g "…"`
//! actually spawns and runs a team.
//!
//! # Shape of a run
//!
//! 1. **Decompose.** One provider call asks the model to split the goal
//!    into N self-contained subtasks, JSON-formatted. If the model
//!    ignores the format, the runner falls back to N copies of the goal
//!    with distinct "focus on a different angle" hints, so a malformed
//!    reply still produces a real swarm rather than a single agent.
//! 2. **Spawn.** One `Agent` per subtask, added to an `AgentSwarm`,
//!    started, and given a registered `Task` through the coordinator.
//! 3. **Run.** All agents call `KodEngine::process_streaming` on their
//!    subtask concurrently. Each has its own channel; the runner
//!    multiplexes them onto one outgoing channel with per-agent labels
//!    so a single consumer sees the whole team's progress in order.
//! 4. **Merge.** One provider call synthesizes the per-agent results
//!    into a single answer. If the call fails — or the config disabled
//!    it — the results are concatenated under their labels.
//!
//! # Concurrency caveats
//!
//! The engine's transcript and short-term memory are engine-global, not
//! per-agent. Concurrent agents interleave their turns into a shared
//! history (bounded by the same cap a single agent gets) and share the
//! steer queue. That is fine for v1 — the swarm's own outputs are what
//! the user reads — but a per-agent transcript is the honest next step
//! if agents are ever expected to see each other's reasoning.

use crate::engine::KodEngine;
use futures::future::join_all;
use kod_error::{KodError, Result};
use kod_provider::{GenerationOptions, LlmProvider};
use kod_swarm::coordination::Task;
use kod_swarm::{AgentBuilder, AgentSwarm, Capability};
use kod_types::{AgentId, Priority, TaskId};
use std::sync::Arc;
use tokio::sync::mpsc;

/// One decomposed piece of the user's goal.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Subtask {
    pub name: String,
    pub description: String,
    /// Path globs the subtask is expected to write (D4.2). Read by
    /// the runner to detect overlapping claims before spawning and
    /// enforced on each agent's `ToolContext::allowed_write_globs`.
    ///
    /// A planner that produces good globs is the difference between
    /// a swarm that parallelizes and one that serializes on a
    /// shared file. The decompose prompt asks for the list; when
    /// the model returns an empty list (or the fallback path is
    /// taken), the runner falls back to a permissive default and
    /// relies on the merge-time conflict detector.
    pub expected_writes: Vec<String>,
    /// Capability the planner assigned to this subtask. The
    /// runner uses it to route the subtask to an endpoint via
    /// `routing.swarm`; the previous inference (`capability_for`)
    /// remains as a fallback for the case where the planner omits
    /// the field.
    pub capability: Capability,
    /// Names of other subtasks in this run that must complete before
    /// this one starts (D4.2). The runner dispatches in dependency
    /// waves; a subtask whose `depends_on` are all complete runs
    /// together with its peers. A subtask that names an unknown
    /// dependency, or sits in a cycle, is promoted to the first
    /// wave with a warning — the merge-time conflict detector is
    /// the second line of defence, and a stuck scheduler would be
    /// worse than a racing subtask.
    pub depends_on: Vec<String>,
}

/// Progress events a swarm run emits while it works. The consumer
/// decides what to render; the runner does not print.
///
/// Derives serde with adjacent tagging (`kind` + `data`) so the
/// daemon can serialize an event as one JSON object without a
/// hand-written match arm per variant. The wire shape is
/// `{"kind": "agent_started", "data": {...}}`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum SwarmEvent {
    /// The decompose step produced these subtasks.
    Decomposed(Vec<Subtask>),
    /// Agent started on its subtask.
    AgentStarted {
        id: AgentId,
        name: String,
        subtask: String,
        /// Display string of the endpoint+model the agent runs
        /// against (`endpoint/model`). Set from
        /// `[llm.routing.swarm]` when configured; falls back to
        /// the engine's current model otherwise, so the agent
        /// panel never shows a blank row.
        #[serde(default)]
        model: Option<String>,
    },
    /// Agent produced a text chunk. Already labeled for display by the
    /// runner (a tool-start marker from the engine is rendered as
    /// `  [tool: read_file]`, not forwarded raw).
    AgentChunk {
        id: AgentId,
        name: String,
        text: String,
    },
    /// Agent finished with this result.
    AgentCompleted {
        id: AgentId,
        name: String,
        result: String,
    },
    /// Agent failed; the swarm continues with the others.
    AgentFailed {
        id: AgentId,
        name: String,
        error: String,
    },
    /// Agent timed out or errored and is being restarted (D4-D6).
    /// Emitted between the failure and the next `AgentStarted` for the
    /// same id, so a live UI can show "agent-1 retrying (2/2): …".
    AgentRetrying {
        id: AgentId,
        name: String,
        attempt: u32,
        max_attempts: u32,
        previous_error: String,
    },
    /// Two agents wrote to the same file. Emitted after all agents
    /// finish, before the merge call, so a live UI can show the
    /// conflict while it is still actionable.
    ConflictDetected { file: String, agents: Vec<String> },
    /// All agents done; the runner is now calling the model to merge.
    Merging,
    /// A per-agent git worktree was created (D4-D2). Emitted before
    /// `AgentStarted` for the same agent so a live UI can show the
    /// isolated workspace the agent will work in.
    WorktreeCreated {
        agent_name: String,
        path: std::path::PathBuf,
        branch: String,
    },
    /// The worktrees were merged back into the base branch. Emitted
    /// after every agent finishes and before the LLM merge step, so
    /// the caller can see the deterministic git result first.
    WorktreesMerged {
        merged: Vec<String>,
        conflicted: Vec<std::path::PathBuf>,
        failed: Vec<(String, String)>,
    },
    /// P5: a subagent wrote a file outside its declared
    /// `expected_writes` globs. Emitted after the report is parsed.
    BoundaryViolation {
        agent_name: String,
        paths: Vec<std::path::PathBuf>,
    },
}

/// Two or more agents touched the same file. The runner detects
/// these after all agents finish, from the canonical path each
/// `write_file` call recorded in its result. A conflict is a warning,
/// not a failure — the merge step is asked to reconcile — but a caller
/// that wants to surface it live reads `SwarmEvent::ConflictDetected`.
#[derive(Debug, Clone)]
pub struct FileConflict {
    pub file: String,
    pub agents: Vec<String>,
}

/// One agent's terminal outcome.
#[derive(Debug, Clone)]
pub struct AgentResult {
    pub id: AgentId,
    pub name: String,
    pub subtask: String,
    pub outcome: AgentOutcome,
}

#[derive(Debug, Clone)]
pub enum AgentOutcome {
    Completed(String),
    Failed(String),
}

/// Full swarm response.
#[derive(Debug, Clone)]
pub struct SwarmResponse {
    pub subtasks: Vec<Subtask>,
    pub per_agent: Vec<AgentResult>,
    /// Files written by two or more agents. The merge step is asked
    /// to reconcile them; the caller decides whether to warn, retry,
    /// or present the merged answer and note the conflict.
    pub conflicts: Vec<FileConflict>,
    /// The merged answer.
    pub merged: String,
    /// True when `merged` came from a synthesis call; false when it is a
    /// concatenation (config disabled, or the synthesis call failed).
    pub merged_by_model: bool,
    /// Git-level worktree merge outcome (D4-D2). `None` when the run
    /// did not use worktrees (a non-git working dir, or a create
    /// failure that forced a fall back to the shared root).
    pub worktree_merge: Option<WorktreeMergeOutcome>,
}

/// What the git merge of every worktree produced.
#[derive(Debug, Clone)]
pub struct WorktreeMergeOutcome {
    /// Branches that merged cleanly, in merge order.
    pub merged: Vec<String>,
    /// Files that produced a conflict; the merge was aborted.
    pub conflicted: Vec<std::path::PathBuf>,
    /// Branches that failed to merge for reasons other than a
    /// conflict (dirty index, missing branch), with the git message.
    pub failed: Vec<(String, String)>,
}

/// Runs a swarm. Construct once per goal; `run` is the only entry point.
pub struct SwarmRunner {
    engine: Arc<KodEngine>,
    provider: Arc<dyn LlmProvider>,
    max_agents: usize,
    merge_results: bool,
    /// Per-agent wall-clock cap; 0 disables.
    agent_timeout_secs: u64,
    /// Additional attempts after a failure.
    agent_retries: u32,
    /// Overall run timeout in seconds. `0` disables it. Unlike the
    /// per-agent cap, this bounds the total wall-clock time of the
    /// entire swarm run, including the merge phase. Default 1800
    /// (30 minutes) — long enough for a large run, short enough that
    /// a hung agent does not wedge a terminal forever.
    swarm_timeout_secs: u64,
}

impl SwarmRunner {
    /// `max_agents` is clamped to `[2, 8]`: one agent is not a swarm,
    /// and more than eight concurrent agentic loops against a local
    /// model server will queue behind each other rather than run
    /// concurrently.
    pub async fn new(
        engine: Arc<KodEngine>,
        max_agents: usize,
        merge_results: bool,
    ) -> Result<Self> {
        let provider = engine.current_provider().await.ok_or_else(|| {
            KodError::InvalidState(
                "SwarmRunner: no LLM provider installed. \
                 Call engine.set_registry(...) first."
                    .to_string(),
            )
        })?;
        Ok(Self {
            engine,
            provider,
            max_agents: max_agents.clamp(2, 8),
            merge_results,
            agent_timeout_secs: 300,
            agent_retries: 1,
            swarm_timeout_secs: 1800,
        })
    }

    /// Build a runner from the loaded `[swarm]` config. Centralises
    /// the config → runner mapping so the two construction sites (CLI
    /// `kod swarm` and TUI `/swarm`) cannot drift on which fields they
    /// apply.
    ///
    /// `max_agents` is clamped by `SwarmRunner::new` to `[2, 8]`; the
    /// other three are applied verbatim, including the "0 disables"
    /// convention for the two timeouts.
    pub async fn from_config(
        engine: Arc<KodEngine>,
        config: &kod_config::SwarmConfig,
    ) -> Result<Self> {
        Ok(Self::new(engine, config.max_agents, config.merge_results)
            .await?
            .with_agent_timeout_secs(config.agent_timeout_secs)
            .with_agent_retries(config.agent_retries)
            .with_swarm_timeout_secs(config.timeout_secs))
    }

    /// Override the agent count (H-C3). The CLI passes `--agents N`;
    /// the pre-fix path ignored it and used the config-only value.
    /// The clamp is applied by `new`, so this setter stores the raw
    /// request; the effective value is still `[2, 8]`.
    pub fn with_max_agents(mut self, n: usize) -> Self {
        self.max_agents = n.clamp(2, 8);
        self
    }

    /// Set the per-agent wall-clock cap in seconds. 0 disables.
    pub fn with_agent_timeout_secs(mut self, secs: u64) -> Self {
        self.agent_timeout_secs = secs;
        self
    }

    /// Set the retry budget. 0 means no retries.
    pub fn with_agent_retries(mut self, retries: u32) -> Self {
        self.agent_retries = retries;
        self
    }

    /// Set the overall run timeout in seconds. 0 disables it.
    pub fn with_swarm_timeout_secs(mut self, secs: u64) -> Self {
        self.swarm_timeout_secs = secs;
        self
    }

    pub fn max_agents(&self) -> usize {
        self.max_agents
    }

    /// Run the swarm to completion. Events are emitted on `chunk_tx`
    /// while the run proceeds; the returned response carries the same
    /// information in a shape a caller can consume without a channel.
    pub async fn run(
        &self,
        goal: &str,
        chunk_tx: &mpsc::Sender<SwarmEvent>,
    ) -> Result<SwarmResponse> {
        // 0. Try to set up worktrees (D4-D2). `None` when the working
        //    directory is not a git repo — the runner then falls back
        //    to the shared root, exactly the pre-D4 behaviour.
        let mut worktree_mgr: Option<crate::worktree::WorktreeManager> =
            match crate::worktree::WorktreeManager::detect(self.engine.working_dir()) {
                Ok(Some(m)) => Some(m),
                Ok(None) => None,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "worktree detect failed; using shared workspace"
                    );
                    None
                }
            };
        if worktree_mgr.is_some() {
            tracing::info!("swarm: worktree mode enabled (per-agent isolation)");
        }

        // 1. Decompose.
        let mut subtasks = self.decompose(goal).await?;

        // 1a. Overlap check (D4.2). A pair of subtasks with
        //     intersecting write claims is a plan the runner cannot
        //     parallelize safely. The remedy is one re-plan — the
        //     hint names the colliding globs so the model
        //     redistributes. A second overlap after the retry is
        //     accepted: the claims are enforced at the tool-call
        //     boundary regardless, and the merge-time conflict
        //     detector is the second line of defence.
        // P4.7 — after the syntactic overlap check, ask Jev
        // whether any pairs touch the same conceptual file even
        // without a shared glob prefix. A semantic hit is logged
        // and (for now) left to the merge-time conflict detector;
        // the flag is available to callers that want to serialize.
        {
            let pairs: Vec<(String, Vec<String>)> = subtasks
                .iter()
                .map(|s| (s.description.clone(), s.expected_writes.clone()))
                .collect();
            let semantic = self.engine.semantic_overlap_check(&pairs).await;
            if !semantic.is_empty() {
                tracing::warn!(
                    count = semantic.len(),
                    "Jev found semantic write overlaps the glob check missed",
                );
            }
        }

        if let Some((i, j, common)) = detect_overlap(&subtasks) {
            tracing::warn!(
                first = %subtasks[i].name,
                second = %subtasks[j].name,
                globs = ?common,
                "swarm: write-claim overlap; asking the planner to redistribute"
            );
            let replan_hint = format!(
                "Your previous reply had two subtasks whose write sets \
                 overlap:\n  \"{}\" claims {}\n  \"{}\" claims {}\n\
                 Paths in the overlap: {}\n\nRedistribute so no two \
                 subtasks touch the same file. Either split the file's \
                 contents along a different axis (a schema, an \
                 interface, a test fixture), or merge the two subtasks \
                 into one.",
                subtasks[i].name,
                subtasks[i]
                    .expected_writes
                    .iter()
                    .map(|s| format!("\"{s}\""))
                    .collect::<Vec<_>>()
                    .join(", "),
                subtasks[j].name,
                subtasks[j]
                    .expected_writes
                    .iter()
                    .map(|s| format!("\"{s}\""))
                    .collect::<Vec<_>>()
                    .join(", "),
                common.join(", "),
            );
            if let Ok(replanned) = self.decompose_with_hint(goal, &replan_hint).await
                && !replanned.is_empty()
            {
                subtasks = replanned;
            }
        }
        let _ = chunk_tx
            .send(SwarmEvent::Decomposed(subtasks.clone()))
            .await;

        // 1b. Create one worktree per subtask. All-or-nothing: a
        //     failure on any worktree drops the manager (cleaning up
        //     whatever was created) and falls back to the shared root.
        let mut worktree_created: Vec<crate::worktree::WorktreeInfo> = Vec::new();
        let mut worktree_failed = false;
        if let Some(mgr) = worktree_mgr.as_mut() {
            for (i, st) in subtasks.iter().enumerate() {
                let slug = format!("agent-{}-{}", i + 1, sanitize(&st.name));
                match mgr.create(&slug) {
                    Ok(info) => worktree_created.push(info),
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "worktree create failed; falling back to shared root"
                        );
                        worktree_failed = true;
                        break;
                    }
                }
            }
        }
        if worktree_failed {
            // `worktree_mgr = None` drops the manager, which cleans up
            // every worktree created before the failure.
            worktree_mgr = None;
            worktree_created.clear();
        }

        // 2. Spawn a capability pool and register tasks (design D4.3).
        //
        // Design: "dispatch = least_loaded_agent parmi
        // find_agents_with_capability(cap)". The pool contains one
        // agent per *distinct* subtask capability, capped at
        // `max_agents`. A run with 3 coding subtasks and 2 testing
        // subtasks spawns at most 2 agents (one coder, one tester),
        // not 5; each agent then takes work from the coordinator via
        // `least_loaded_agent` filtering on `find_agents_with_capability`.
        //
        // A run where every subtask has a distinct capability still
        // spawns one agent per subtask — identical to the pre-D4.3
        // behaviour, because the pool size equals the subtask count.
        // The change only collapses redundant same-capability agents,
        // which is the exact class of work a single agent can absorb
        // without loss (the design's "coordination réelle au lieu de
        // comptabilité").
        let hub = self.engine.swarm_hub();
        let swarm = AgentSwarm::with_hub(hub.clone());

        // Distinct capabilities, in first-appearance order so the
        // pool's naming is deterministic.
        let mut capabilities: Vec<Capability> = Vec::new();
        for st in &subtasks {
            if !capabilities.contains(&st.capability) {
                capabilities.push(st.capability);
            }
        }
        // Capability pool capped at max_agents; if the pool would
        // exceed the cap, fall back to one agent per subtask (the
        // pre-D4.3 shape) so no subtask is left without a coder.
        let pool: Vec<Capability> = if capabilities.len() <= self.max_agents {
            capabilities.clone()
        } else {
            subtasks.iter().map(|s| s.capability).collect()
        };

        // Map capability -> the agents that can serve it. Populated as
        // we spawn. `Vec` (not `HashSet`) to keep the deterministic
        // order that `find_agents_with_capability` sorts on.
        let mut capability_agents: std::collections::HashMap<Capability, Vec<AgentId>> =
            std::collections::HashMap::new();

        // Per-agent handle info: name and, when a worktree was created,
        // its path. Keyed by AgentId so the wave loop can find the right
        // worktree when it assigns a subtask to an agent.
        struct AgentHandle {
            /// Dispatch id — unique per subtask. Two subtasks assigned
            /// to the same pool agent get distinct ids, so their
            /// transcripts (keyed `swarm:{id}`) cannot collide. H-D1:
            /// pre-fix both used the pool agent id, so one subtask's
            /// failure path wiped its peer's history mid-flight.
            id: AgentId,
            /// The pool agent that owns this subtask (load-balancing
            /// bookkeeping; distinct from `id`).
            pool_agent_id: AgentId,
            name: String,
            subtask: Subtask,
            task_id: TaskId,
            /// Worktree path this subtask runs in, if any. Set at
            /// dispatch time (not pool time) because the transcript
            /// key is subtask-scoped.
            worktree: Option<crate::worktree::WorktreeInfo>,
        }

        // H-D1: the watchdog needs to reach every *dispatch* key
        // currently running on a given pool agent. This map is the
        // link between the two identities. Shared between the wave
        // loop (inserts/removes) and the watchdog (iterates).
        let dispatch_keys: Arc<
            parking_lot::Mutex<std::collections::HashMap<AgentId, Vec<String>>>,
        > = Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));

        let mut pool_handles: Vec<AgentHandle> = Vec::with_capacity(pool.len());
        for (i, cap) in pool.iter().enumerate() {
            let slug = format!("agent-{}-{}", i + 1, sanitize(cap.as_str()));
            let agent = AgentBuilder::new(&slug).with_capability(*cap).build();
            let id = agent.id().clone();
            swarm.add_agent(agent).await?;
            swarm.start_agent(&id).await?;

            // Point this agent's transcript at its own worktree, if one
            // was created. The per-transcript working_dir override
            // (D4-D1) makes every tool the agent calls run rooted there.
            // H-D1: the transcript-keyed per-agent state
            // (working_dir, write_globs) is applied at *dispatch*
            // time on the subtask-scoped dispatch id, not here. We
            // still surface the WorktreeCreated event so the UI knows
            // the pool agent owns a worktree.
            let pool_worktree = worktree_created.get(i).cloned();
            if let Some(wt) = &pool_worktree {
                let _ = chunk_tx
                    .send(SwarmEvent::WorktreeCreated {
                        agent_name: slug.clone(),
                        path: wt.path.clone(),
                        branch: wt.branch.clone(),
                    })
                    .await;
            }

            capability_agents.entry(*cap).or_default().push(id.clone());
            pool_handles.push(AgentHandle {
                id: id.clone(),
                pool_agent_id: id,
                name: slug,
                // Placeholder subtask — the runner assigns the real
                // one in the wave loop. Kept non-empty so the
                // pre-existing display code that reads `subtask` has
                // something sensible.
                subtask: Subtask {
                    name: String::new(),
                    description: String::new(),
                    expected_writes: Vec::new(),
                    capability: *cap,
                    depends_on: Vec::new(),
                },
                task_id: TaskId::new(),
                worktree: pool_worktree,
            });
        }

        // Register one task per subtask, assign it to the
        // least-loaded agent of the right capability. The wave loop
        // below re-registers nothing — it just consumes the handles
        // that have already been paired with their subtask.
        //
        // `handles` (the pre-pool vector) is what the wave loop
        // dispatches on. Rebuild it here from `subtasks` and the pool
        // so the rest of the file does not change.
        let mut handles: Vec<AgentHandle> = Vec::with_capacity(subtasks.len());
        for st in &subtasks {
            let candidates = capability_agents
                .get(&st.capability)
                .cloned()
                .unwrap_or_default();
            let chosen: AgentId = match candidates.len() {
                0 => {
                    // No pool agent declared this capability — should
                    // not happen: `pool` was built from subtask
                    // capabilities. Defensive: fall back to the first
                    // agent.
                    pool_handles
                        .first()
                        .map(|h| h.id.clone())
                        .unwrap_or_default()
                }
                1 => candidates[0].clone(),
                _ => {
                    // Real load-balancing: ask the coordinator which
                    // candidate has the fewest in-flight tasks.
                    swarm
                        .coordinator()
                        .least_loaded_agent(&candidates)
                        .await
                        .unwrap_or_else(|| candidates[0].clone())
                }
            };
            let (name, worktree) = pool_handles
                .iter()
                .find(|h| h.id == chosen)
                .map(|h| (h.name.clone(), h.worktree.clone()))
                .unwrap_or_else(|| (format!("agent-{}", st.name), None));

            let task = Task::new(st.description.clone(), Priority::Medium);
            let task_id = task.id.clone();
            swarm.coordinator().register_task(task).await?;
            swarm.coordinator().assign_task(&task_id, &chosen).await?;

            // H-D1: mint a *fresh* dispatch id for this subtask. The
            // engine keys its per-transcript state (history, cancel
            // flag, working dir, write globs, blackboard viewer) on
            // `swarm:{id}`; giving each subtask its own id means two
            // concurrently-running subtasks on the same pool agent
            // cannot collide.
            let dispatch_id = AgentId::new();
            let dispatch_key = format!("swarm:{dispatch_id}");

            // Per-transcript working dir: the pool agent's worktree,
            // if any.
            if let Some(wt) = &worktree {
                let _ = self
                    .engine
                    .set_transcript_working_dir(&dispatch_key, Some(wt.path.clone()))
                    .await;
            }

            // Per-transcript write set.
            if !st.expected_writes.is_empty() {
                let _ = self
                    .engine
                    .set_transcript_write_globs(&dispatch_key, Some(st.expected_writes.clone()))
                    .await;
                // Tier 3.5 — subscribe this agent to the shared
                // blackboard so its prompt includes what the team knows.
                let _ = self.engine.set_blackboard_viewer(&dispatch_key, true).await;
            }

            handles.push(AgentHandle {
                id: dispatch_id,
                pool_agent_id: chosen,
                name,
                subtask: st.clone(),
                task_id,
                worktree,
            });
        }

        // 3. Run all agents concurrently. Each gets its own channel so
        //    the engine streams per-agent text; a small forwarding task
        //    relabels tool markers and pushes onto the outgoing channel.
        // Per-agent timeout / retry (D4-D6). The timeout bounds a
        // stuck agent; the retry budget restarts it once with the
        // previous error injected into the prompt. Both are configured
        // via SwarmConfig (default 300 s, 1 retry).
        let agent_timeout_secs: u64 = self.agent_timeout_secs;
        let max_attempts: u32 = 1 + self.agent_retries;

        // Dispatch in dependency waves (D4.2). Subtasks whose
        // `depends_on` are all complete run together; a subtask
        // whose dependencies have not yet run waits for the next
        // wave. A subtask that names an unknown dependency, or sits
        // in a cycle, is promoted to the current wave with a
        // warning — the merge-time conflict detector is the second
        // line of defence, and a stuck scheduler would be worse
        // than a racing subtask.
        let mut raw: Vec<(
            kod_types::AgentId,
            String,
            Subtask,
            Vec<String>,
            std::result::Result<String, String>,
        )> = Vec::with_capacity(handles.len());
        let mut completed: std::collections::HashSet<String> = std::collections::HashSet::new();
        // Per-run heartbeat watchdog (design D4.3). Polls every 10 s;
        // for any agent whose last heartbeat is older than 90 s, sends
        // a cooperative cancel so the agent's streaming loop stops at
        // its next round boundary and its task exits with a retryable
        // error — the retry loop then restarts it, which is the
        // design's "re-dispatch". Best-effort: the watchdog holds an
        // engine handle and a swarm handle, both of which the run
        // already owns.
        let (watchdog_stop_tx, mut watchdog_stop_rx) = tokio::sync::mpsc::channel::<()>(1);
        let watchdog_engine = self.engine.clone();
        let watchdog_swarm = swarm.clone();
        let watchdog_chunk_tx = chunk_tx.clone();
        let watchdog_dispatch_keys = dispatch_keys.clone();
        // H-R6: derive the idle threshold from the configured agent
        // timeout instead of the hardcoded 90 s. A user who raised
        // `agent_timeout_secs = 1800` for slow tools (cargo build on
        // a cold cache) still got cancelled at 90 s; the watchdog
        // ignored the setting entirely.
        //
        // 90 s is the floor (in case `agent_timeout_secs` is 0 =
        // disabled — watchdog still functions), and 1/4 of the
        // agent timeout is the ceiling so a healthy-but-slow agent
        // is never cancelled before its real timeout could fire.
        let watchdog_idle_secs: u64 = if agent_timeout_secs == 0 {
            90
        } else {
            (agent_timeout_secs / 4).max(90).min(agent_timeout_secs)
        };
        let watchdog_task = tokio::spawn(async move {
            use tokio::time::{Duration, MissedTickBehavior};
            let mut ticker = tokio::time::interval(Duration::from_secs(10));
            ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
            // Skip the immediate first tick so the initial spawn does
            // not race the pool registration.
            ticker.tick().await;
            loop {
                tokio::select! {
                    _ = ticker.tick() => {}
                    _ = watchdog_stop_rx.recv() => return,
                }
                let ids = watchdog_swarm.list_agents().await;
                for id in ids {
                    let Some(agent) = watchdog_swarm.get_agent(&id).await else {
                        continue;
                    };
                    // Only Running agents are eligible; a Stopped one
                    // has no heartbeat obligation.
                    if agent.state() != kod_swarm::AgentState::Running {
                        continue;
                    }
                    // 90 s without a chunk: the agent is wedged on
                    // something (a hung tool, a stalled stream). Cancel
                    // cooperatively so the retry loop can restart it.
                    if agent.is_timed_out(Duration::from_secs(watchdog_idle_secs)) {
                        // H-D1: cancel every dispatch key running on
                        // this pool agent, not the pool agent's own
                        // name. The engine keys its cancellation set
                        // by transcript — one dispatch per subtask.
                        let keys: Vec<String> = watchdog_dispatch_keys
                            .lock()
                            .get(&id)
                            .cloned()
                            .unwrap_or_default();
                        if keys.is_empty() {
                            // No live dispatch: the pool agent is
                            // between subtasks. Nothing to cancel.
                            continue;
                        }
                        for key in keys {
                            tracing::warn!(
                                agent = %agent.name(),
                                key = %key,
                                "swarm watchdog: dispatch has not reported within the idle window;                                  sending cooperative cancel",
                            );
                            watchdog_engine.request_cancel_for(&key);
                            let _ = watchdog_chunk_tx
                                .send(SwarmEvent::AgentRetrying {
                                    id: id.clone(),
                                    name: agent.name().to_string(),
                                    attempt: 0,
                                    max_attempts: 0,
                                    previous_error: "watchdog: no output in 90s".to_string(),
                                })
                                .await;
                        }
                    }
                }
            }
        });

        let mut remaining: Vec<usize> = (0..handles.len()).collect();
        let mut guard = handles.len() + 1;

        // Global run deadline (design D4.3). The per-agent timeout
        // above bounds one agent; without a ceiling on the whole run,
        // a wave-per-`agent_timeout_secs` sequence (retries included)
        // could run for hours. `swarm_timeout_secs == 0` disables the
        // cap. The deadline is absolute so a wave cannot reset it.
        let global_deadline = if self.swarm_timeout_secs == 0 {
            None
        } else {
            Some(
                tokio::time::Instant::now()
                    + std::time::Duration::from_secs(self.swarm_timeout_secs),
            )
        };

        // Emit an `AgentFailed` for every handle in `indices` that has
        // not been recorded as completed. Used by both the top-of-loop
        // deadline check and the wave-level timeout.
        async fn report_run_timeout(
            chunk_tx: &mpsc::Sender<SwarmEvent>,
            handles: &[AgentHandle],
            indices: &[usize],
            timeout_secs: u64,
        ) {
            let reason = format!(
                "overall run timeout ({}s) reached; this agent did not complete",
                timeout_secs,
            );
            for &idx in indices {
                if let Some(h) = handles.get(idx) {
                    let _ = chunk_tx
                        .send(SwarmEvent::AgentFailed {
                            id: h.id.clone(),
                            name: h.name.clone(),
                            error: reason.clone(),
                        })
                        .await;
                }
            }
            // Synthetic run-level event so a live UI can distinguish
            // "this agent failed" from "the whole run stopped".
            let _ = chunk_tx
                .send(SwarmEvent::AgentFailed {
                    id: kod_types::AgentId::new(),
                    name: "(run)".to_string(),
                    error: format!("overall run timeout ({}s) reached", timeout_secs,),
                })
                .await;
        }

        // P5: build the parent's context once for the whole run.
        // The snapshot is taken here so every subtask's brief draws
        // from the same state — the parent does not change while the
        // swarm works, and a per-wave rebuild would produce slightly
        // different briefs for logically-peer subtasks.
        //
        // `AgentHandle` is a local struct, so this block lives inline
        // rather than in a module-level helper.
        let parent_context = {
            let decisions = self.engine.recent_decisions("", 16).await;
            let repomap_text = self.engine.repomap_text().await;
            let expected_writes: Vec<String> = handles
                .iter()
                .flat_map(|h| h.subtask.expected_writes.iter().cloned())
                .collect();
            let mut file_summaries = Vec::new();
            let mut seen = std::collections::HashSet::new();
            let working_dir = self.engine.working_dir();
            for h in &handles {
                for token in h.subtask.description.split_whitespace() {
                    let cleaned = token.trim_matches(|c: char| {
                        !c.is_alphanumeric() && c != '/' && c != '.' && c != '_' && c != '-'
                    });
                    if cleaned.contains('.') || cleaned.contains('/') {
                        let path = working_dir.join(cleaned);
                        if path.is_file()
                            && seen.insert(path.clone())
                            && let Ok(d) =
                                kod_swarm::brief_assembly::digest_file(&path)
                            && file_summaries.len() < 12
                        {
                            file_summaries.push((d.path, d.summary, d.line_count));
                        }
                    }
                }
            }
            tracing::debug!(
                decisions = decisions.len(),
                digests = file_summaries.len(),
                repomap_chars = repomap_text.len(),
                "P5: parent context built for swarm run",
            );
            kod_swarm::brief_assembly::ParentContext {
                decisions,
                file_summaries,
                repomap_text,
                expected_writes,
                token_budget: 4096,
            }
        };

        while !remaining.is_empty() && guard > 0 {
            guard -= 1;

            // Deadline check first: cheaper to bail than to spawn a
            // wave whose futures will be dropped a moment later.
            if let Some(deadline) = global_deadline
                && tokio::time::Instant::now() >= deadline
            {
                tracing::warn!(
                    secs = self.swarm_timeout_secs,
                    "swarm: overall run deadline reached before a wave;                      aborting remaining agents"
                );
                report_run_timeout(chunk_tx, &handles, &remaining, self.swarm_timeout_secs).await;
                break;
            }
            let (ready, blocked): (Vec<usize>, Vec<usize>) =
                remaining.into_iter().partition(|&i| {
                    handles[i]
                        .subtask
                        .depends_on
                        .iter()
                        .all(|d| completed.contains(d))
                });
            let (ready, blocked) = if ready.is_empty() {
                tracing::warn!(
                    count = blocked.len(),
                    "swarm: dependency cycle or unknown dependency; \
                     running remaining subtasks concurrently"
                );
                (blocked, Vec::new())
            } else {
                (ready, blocked)
            };

            let mut wave_tasks = Vec::with_capacity(ready.len());
            for &i in &ready {
                let engine = self.engine.clone();
                let id = handles[i].id.clone();
                let pool_agent_id = handles[i].pool_agent_id.clone();
                let name = handles[i].name.clone();
                let subtask = handles[i].subtask.clone();
                let parent_context = parent_context.clone();
                let out = chunk_tx.clone();
                let hub = hub.clone();
                let swarm = swarm.clone();
                let dispatch_keys = dispatch_keys.clone();
                // H-D1: register this dispatch key before the task
                // starts so the watchdog can find it. The task
                // removes the entry on every return path.
                {
                    let mut m = dispatch_keys.lock();
                    m.entry(pool_agent_id.clone())
                        .or_default()
                        .push(format!("swarm:{id}"));
                }
                wave_tasks.push(async move {
                // H-D1: guard the deregistration even on panic.
                struct DispatchGuard {
                    keys: Arc<
                        parking_lot::Mutex<std::collections::HashMap<AgentId, Vec<String>>>,
                    >,
                    pool: AgentId,
                    key: String,
                }
                impl Drop for DispatchGuard {
                    fn drop(&mut self) {
                        let mut m = self.keys.lock();
                        if let Some(v) = m.get_mut(&self.pool) {
                            v.retain(|k| k != &self.key);
                            if v.is_empty() {
                                m.remove(&self.pool);
                            }
                        }
                    }
                }
                let _dispatch_guard = DispatchGuard {
                    keys: dispatch_keys,
                    pool: pool_agent_id.clone(),
                    key: format!("swarm:{id}"),
                };

                // Role preamble is computed once — retries use the
                // same shaped prompt.
                // Planner-assigned, not re-inferred — a planner that labelled
                // the subtask as CodeReview gets the reviewer preamble.
                let per_agent_role = subtask.capability;
                let role_prefix = role_preamble(per_agent_role);
                let transcript_key = format!("swarm:{id}");

                // Resolve the per-capability model once per agent
                // (design D1.4 PR A7). `[llm.routing.swarm]` maps a
                // subtask's `capability` to an endpoint; `None` when
                // the table is absent or has no entry — the engine
                // then routes by task type, the pre-A7 behaviour.
                // Computed here rather than inside the retry loop so
                // the agent panel sees the model on the first
                // `AgentStarted` emit of every attempt.
                let override_model: Option<kod_provider::ModelRef> = engine
                    .resolve_model_ref_for_capability(&subtask.capability)
                    .await;

                // Announce this agent's start to its peers before it
                // begins work (D4.3). A terminal-only lifecycle left
                // an agent whose subtask depends on another's output
                // with no way to know a peer was working on it; a
                // start broadcast closes that gap. Best-effort: a hub
                // that is not yet populated accepts the broadcast
                // silently.
                let _ = hub
                    .broadcast_lifecycle(
                        &id,
                        &format!(
                            "started: {}",
                            subtask.description.lines().next().unwrap_or("")
                        ),
                    )
                    .await;

                let mut last_error: Option<String> = None;
                let mut attempt: u32 = 0;
                loop {
                    attempt += 1;
                    let _ = out
                        .send(SwarmEvent::AgentStarted {
                        id: id.clone(),
                        name: name.clone(),
                        subtask: subtask.description.clone(),
                        // Same per-capability model the streaming
                        // call will use. Falls back to the current
                        // model when no `[llm.routing.swarm]` table
                        // is configured — the panel never shows a
                        // blank, and a user can always tell which
                        // endpoint an agent is on.
                        model: Some(
                            match &override_model {
                                Some(m) => m.display(),
                                None => engine.current_model().await.display(),
                            },
                        ),
                    }
                    )
                        .await;

                    // Retry attempts get the previous error appended so
                    // the model can react to it.
                    // P5: build a typed brief from the parent
                    // context. The role preamble is the first
                    // constraint; retry attempts add the previous
                    // error as an extra constraint.
                    let extra_constraints = match &last_error {
                        Some(err) => vec![format!(
                            "The previous attempt failed with: {err}. \
                             Avoid the failure mode above and try a different approach.",
                        )],
                        None => Vec::new(),
                    };
                    let brief = kod_swarm::brief_assembly::assemble_brief(
                        &subtask.description,
                        role_prefix,
                        extra_constraints,
                        &parent_context,
                    );
                    let shaped = kod_swarm::brief::render_brief(&brief);

                    let (tx, mut rx) = mpsc::channel::<String>(64);
                    let out_pump = out.clone();
                    let id_pump = id.clone();
                    let name_pump = name.clone();
                    // Keep a handle to the agent so the pump can record
                    // a heartbeat on every chunk (design D4.3). "The
                    // agent produced output" is the signal the watchdog
                    // watches; the alternative — polling the engine's
                    // last-chunk timestamp — would need a second
                    // cross-task slot for no gain.
                    let swarm_for_hb = swarm.clone();
                    let id_for_hb = pool_agent_id.clone();
                    let pump = tokio::spawn(async move {
                        // The swarm's registry is shared; a lookup per
                        // chunk is a HashMap get. Cheap, and the pump
                        // already crosses an await boundary per chunk
                        // (the mpsc recv), so no extra yield point.
                        //
                        // H-D1: heartbeats live on the *pool* agent
                        // (the one registered in the swarm); the
                        // dispatch id is an engine-transcript-only
                        // identity.
                        while let Some(chunk) = rx.recv().await {
                            if let Some(agent) = swarm_for_hb.get_agent(&id_for_hb).await {
                                agent.record_heartbeat();
                            }
                            let display = if let Some(tool) =
                                crate::engine::parse_tool_start(&chunk)
                            {
                                format!("  [tool: {tool}]\n")
                            } else if let Some(brief) =
                                crate::engine::parse_tool_args(&chunk)
                            {
                                format!("  [{brief}]\n")
                            } else if crate::engine::parse_tool_done(&chunk).is_some()
                                || crate::engine::is_thinking_marker(&chunk)
                            {
                                continue;
                            } else {
                                chunk
                            };
                            let _ = out_pump
                                .send(SwarmEvent::AgentChunk {
                                    id: id_pump.clone(),
                                    name: name_pump.clone(),
                                    text: display,
                                })
                                .await;
                        }
                    });

                    let run = engine
                        .process_streaming_with_model_for(
                            &transcript_key,
                            &shaped,
                            &tx,
                            override_model.clone(),
                        );

                    let outcome: std::result::Result<
                        crate::router::TaskResponse,
                        String,
                    > = if agent_timeout_secs == 0 {
                        run.await.map_err(|e| e.to_string())
                    } else {
                        match tokio::time::timeout(
                            std::time::Duration::from_secs(agent_timeout_secs),
                            run,
                        )
                        .await
                        {
                            Ok(Ok(resp)) => Ok(resp),
                            Ok(Err(e)) => Err(e.to_string()),
                            Err(_) => Err(format!(
                                "timed out after {agent_timeout_secs}s"
                            )),
                        }
                    };

                    // Drop the chunk sender, wait for the pump to
                    // drain, and clear the transcript + cancel flag
                    // before any retry reuses the key.
                    drop(tx);
                    let _ = pump.await;
                    engine.forget_transcript(&transcript_key).await;
                    engine.set_blackboard_viewer(&transcript_key, false).await;
                    engine.clear_cancel_for(&transcript_key);

                    match outcome {
                        Ok(resp) => {
                            let writes = collect_writes(&resp);
                            let text = resp.text.unwrap_or_default();
                            return (id, name, subtask, writes, Ok(text));
                        }
                        Err(err) => {
                            // Signal any in-flight loop to stop cleanly
                            // (the timeout already dropped the future,
                            // but a cooperative cancel is cheap and
                            // makes the next-attempt state unambiguous).
                            engine.request_cancel_for(&transcript_key);
                            last_error = Some(err.clone());
                            if attempt >= max_attempts {
                                return (id, name, subtask, Vec::new(), Err(err));
                            }
                            // Announce the retry so a live UI can show
                            // "agent-1 retrying (2/2): …".
                            let _ = out
                                .send(SwarmEvent::AgentRetrying {
                                    id: id.clone(),
                                    name: name.clone(),
                                    attempt: attempt + 1,
                                    max_attempts,
                                    previous_error: err.clone(),
                                })
                                .await;
                            // Tell the peers this agent is retrying
                            // (D4.3). A peer that is waiting on a
                            // fact this agent was going to produce
                            // learns there is a delay, not a silent
                            // failure.
                            let _ = hub
                                .broadcast_lifecycle(
                                    &id,
                                    &format!(
                                        "retrying ({}/{}): {}",
                                        attempt + 1,
                                        max_attempts,
                                        err.lines().next().unwrap_or("")
                                    ),
                                )
                                .await;
                        }
                    }
                }
            });
            }
            // Run this wave under whatever budget remains. A wave
            // that starts just under the deadline cannot overrun it.
            let wave_results = match global_deadline {
                Some(deadline) => {
                    let remaining_time =
                        deadline.saturating_duration_since(tokio::time::Instant::now());
                    if remaining_time.is_zero() {
                        // The top-of-loop check should have caught
                        // this; be defensive.
                        report_run_timeout(chunk_tx, &handles, &ready, self.swarm_timeout_secs)
                            .await;
                        break;
                    }
                    match tokio::time::timeout(remaining_time, join_all(wave_tasks)).await {
                        Ok(v) => v,
                        Err(_) => {
                            tracing::warn!(
                                secs = self.swarm_timeout_secs,
                                "swarm: wave hit the global deadline;                                  aborting agents mid-flight"
                            );
                            // `ready` futures were dropped by the
                            // timeout; `blocked` never started. Both
                            // need a terminal event.
                            let mut all: Vec<usize> = ready.clone();
                            all.extend(blocked.iter().copied());
                            report_run_timeout(chunk_tx, &handles, &all, self.swarm_timeout_secs)
                                .await;
                            break;
                        }
                    }
                }
                None => join_all(wave_tasks).await,
            };
            for (idx, res) in ready.iter().zip(wave_results) {
                completed.insert(handles[*idx].subtask.name.clone());
                raw.push(res);
            }
            remaining = blocked;
        }

        // `raw` was populated by the dependency-wave loop above; each
        // wave waited up to `agent_timeout_secs` per agent. A per-run
        // global watchdog (the design's D4.3) is a follow-up; until
        // then the wave loop is the only bound.

        // 3b. Merge the worktrees back into the base branch (D4-D2).
        //     The merge is deterministic (git, not a model call) and
        //     runs before the LLM synthesis so the caller sees the
        //     conflict list first. A conflict aborts the merge cleanly
        //     and the report names the files.
        let worktree_merge: Option<WorktreeMergeOutcome> = if let Some(mgr) = worktree_mgr.as_mut()
        {
            match mgr.merge_all() {
                Ok(report) => {
                    let _ = chunk_tx
                        .send(SwarmEvent::WorktreesMerged {
                            merged: report.merged.clone(),
                            conflicted: report.conflicted.clone(),
                            failed: report.failed.clone(),
                        })
                        .await;
                    Some(WorktreeMergeOutcome {
                        merged: report.merged,
                        conflicted: report.conflicted,
                        failed: report.failed,
                    })
                }
                Err(e) => {
                    tracing::warn!(error = %e, "worktree merge failed");
                    None
                }
            }
        } else {
            None
        };

        // 3c. Clear the per-transcript working dir overrides and
        //     write sets so a reused engine (a second swarm run)
        //     does not inherit a stale worktree path or a stale
        //     claim.
        for h in &handles {
            let key = format!("swarm:{}", h.id);
            let _ = self.engine.clear_transcript_working_dir(&key).await;
            self.engine.clear_transcript_write_globs(&key).await;
        }

        // 4. Report terminal status. The coordinator's load accounting
        //    needs the completion, and the events let a live UI update.
        let mut per_agent = Vec::with_capacity(raw.len());
        // File -> agent names that wrote it. Populated as the results
        // come back; entries with two or more are conflicts.
        let mut writers_per_file: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        for (id, name, subtask, writes, outcome) in raw {
            // H-D3: normalize each write path to the *logical* file
            // relative to the repo root, stripping any per-agent
            // worktree prefix. In worktree mode two agents editing
            // the same file record different absolute paths
            // (`<repo>/.kod/worktrees/agent-1/src/x.rs` vs
            // `<repo>/.kod/worktrees/agent-2/src/x.rs`), so the
            // pre-fix bucket saw no overlap and the merge prompt was
            // never told. Stripping the worktree prefix makes the two
            // bucket under the same key.
            let worktree_prefix: Option<std::path::PathBuf> = handles
                .iter()
                .find(|h| h.id == id)
                .and_then(|h| h.worktree.as_ref().map(|w| w.path.clone()));
            for raw_path in &writes {
                let normalized = match &worktree_prefix {
                    Some(prefix) => std::path::Path::new(raw_path)
                        .strip_prefix(prefix)
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_else(|_| raw_path.clone()),
                    None => raw_path.clone(),
                };
                writers_per_file
                    .entry(normalized)
                    .or_default()
                    .push(name.clone());
            }
            // Find the matching handle's task id by agent id.
            let task_id = handles
                .iter()
                .find(|h| h.id == id)
                .map(|h| h.task_id.clone());
            match outcome {
                Ok(text) => {
                    if let Some(tid) = task_id.as_ref() {
                        let _ = swarm.coordinator().complete_task(tid).await;
                    }
                    // Record the completion on the hub so peers can
                    // see it (D4-D3b). Best-effort: a hub error never
                    // fails the run.
                    let _ = swarm
                        .communication()
                        .broadcast_lifecycle(
                            &id,
                            &format!("completed: {}", text.lines().next().unwrap_or("(no text)")),
                        )
                        .await;
                    let _ = chunk_tx
                        .send(SwarmEvent::AgentCompleted {
                            id: id.clone(),
                            name: name.clone(),
                            result: text.clone(),
                        })
                        .await;
                    per_agent.push(AgentResult {
                        id,
                        name,
                        subtask: subtask.description,
                        outcome: AgentOutcome::Completed(text),
                    });
                }
                Err(e) => {
                    if let Some(tid) = task_id.as_ref() {
                        let _ = swarm.coordinator().fail_task(tid).await;
                    }
                    let _ = swarm
                        .communication()
                        .broadcast_lifecycle(
                            &id,
                            &format!("failed: {}", e.lines().next().unwrap_or("")),
                        )
                        .await;
                    let _ = chunk_tx
                        .send(SwarmEvent::AgentFailed {
                            id: id.clone(),
                            name: name.clone(),
                            error: e.clone(),
                        })
                        .await;
                    per_agent.push(AgentResult {
                        id,
                        name,
                        subtask: subtask.description,
                        outcome: AgentOutcome::Failed(e),
                    });
                }
            }
        }

        // 5. Conflicts. A file written by two or more agents is a
        //    signal the merge step needs to reconcile; it is not an
        //    error — agents on interdependent subtasks legitimately
        //    touch the same file, and the model is the right place to
        //    decide what "merged" means.
        let mut conflicts: Vec<FileConflict> = Vec::new();
        for (file, mut agents) in writers_per_file {
            agents.sort();
            agents.dedup();
            if agents.len() >= 2 {
                conflicts.push(FileConflict {
                    file: file.clone(),
                    agents: agents.clone(),
                });
                let _ = chunk_tx
                    .send(SwarmEvent::ConflictDetected { file, agents })
                    .await;
            }
        }
        // Stable order so a caller reading `conflicts` sees the same
        // list across runs.
        conflicts.sort_by(|a, b| a.file.cmp(&b.file));

        // 6. Merge.
        let (merged, merged_by_model) = if self.merge_results {
            let _ = chunk_tx.send(SwarmEvent::Merging).await;
            match self.merge(goal, &per_agent, &conflicts).await {
                Ok(s) if !s.trim().is_empty() => (s, true),
                _ => (Self::concatenate(&per_agent), false),
            }
        } else {
            (Self::concatenate(&per_agent), false)
        };

        // The run is over; stop the watchdog (drop the sender, await
        // the task) so it cannot fire against a completed run.
        let _ = watchdog_stop_tx.send(()).await;
        let _ = watchdog_task.await;

        // H-R5: tear down every agent + the shared hub. The engine
        // owns one long-lived hub, so without this step every run
        // leaks its agents (registered, with unbounded mpsc inboxes
        // and 100-message histories). In a long-lived daemon that
        // grows without bound.
        //
        // `swarm.shutdown()` stops each agent and unregisters it
        // from the hub; `clear_all()` drops the hub's own per-agent
        // maps. Both are best-effort — a shutdown error is logged
        // and the run still returns its result.
        if let Err(e) = swarm.shutdown().await {
            tracing::warn!(error = %e, "swarm shutdown reported an error");
        }
        hub.clear_all().await;

        Ok(SwarmResponse {
            subtasks,
            per_agent,
            conflicts,
            merged,
            merged_by_model,
            worktree_merge,
        })
    }

    /// Probe the working directory for context the decompose prompt
    /// can use.
    ///
    /// Two cheap tool calls: a shallow `list_files` for the tree, and a
    /// `grep` per content-word from the goal for where the goal's
    /// vocabulary already appears. Both are bounded — `list_files` is
    /// capped by the tool, and the probe takes the first few hits per
    /// keyword — so a large repository produces a summary the model can
    /// read in one paragraph, not a dump.
    ///
    /// Returns `None` when neither call produces anything useful (a
    /// non-repo directory, a tool failure). The decompose prompt handles
    /// the absent case with a one-line placeholder; there is no reason
    /// to fail the whole decompose because the working directory is
    /// empty.
    async fn probe_repo(&self, goal: &str) -> Option<String> {
        use kod_types::ToolResult;

        // ---- Shallow listing. ----
        let listing = self
            .engine
            .run_tool(
                "list_files",
                serde_json::json!({ "path": ".", "recursive": false }),
            )
            .await
            .ok()?;
        let listing_names: Vec<String> = match listing {
            ToolResult::Success(v) => v
                .get("files")
                .and_then(|f| f.as_array())
                .map(|a| {
                    a.iter()
                        .take(40)
                        .filter_map(|v| v.as_str())
                        .map(|s| {
                            // The walker returns absolute paths; strip the
                            // working-dir prefix so the model sees names.
                            s.rsplit('/').next().unwrap_or(s).to_string()
                        })
                        .collect()
                })
                .unwrap_or_default(),
            _ => return None,
        };
        if listing_names.is_empty() {
            return None;
        }

        // ---- Keyword grep. ----
        // Content words: length >= 4, deduplicated, capped at 5 so the
        // probe stays bounded on a long goal.
        let mut seen = std::collections::HashSet::new();
        let keywords: Vec<String> = goal
            .to_lowercase()
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| w.len() >= 4)
            .filter(|w| seen.insert(w.to_string()))
            .take(5)
            .map(|s| s.to_string())
            .collect();

        let mut hits = String::new();
        for kw in &keywords {
            let grep = match self
                .engine
                .run_tool(
                    "grep",
                    serde_json::json!({ "path": ".", "pattern": kw, "recursive": true }),
                )
                .await
            {
                Ok(ToolResult::Success(v)) => v,
                _ => continue,
            };
            let results = match grep.get("results").and_then(|r| r.as_array()) {
                Some(a) if !a.is_empty() => a,
                _ => continue,
            };
            let files: Vec<&str> = results
                .iter()
                .take(5)
                .filter_map(|r| r.get("file").and_then(|f| f.as_str()))
                .map(|s| s.rsplit('/').next().unwrap_or(s))
                .collect();
            hits.push_str(&format!(
                "  \"{}\" ({} match{}): {}\n",
                kw,
                results.len(),
                if results.len() == 1 { "" } else { "es" },
                files.join(", ")
            ));
        }

        let mut out = String::new();
        out.push_str("Top-level entries:\n");
        out.push_str(&listing_names.join(", "));
        if !hits.is_empty() {
            out.push_str("\n\nGoal keywords already present in the tree:\n");
            out.push_str(&hits);
        }
        Some(out)
    }

    /// Decompose with an extra instruction appended to the prompt.
    /// Used by the overlap re-plan to tell the model what went
    /// wrong without rebuilding the whole prompt.
    ///
    /// Reuses `decompose`'s probe and prompt, appends the hint as a
    /// postscript. A failure to reach the model returns the error
    /// so the caller can decide whether to retry with the original
    /// plan or abort; today the only caller falls through to the
    /// original plan, which is the safe default.
    async fn decompose_with_hint(&self, goal: &str, hint: &str) -> Result<Vec<Subtask>> {
        // The probe is best-effort inside `decompose` — here we
        // simply prepend a hint line and call the underlying
        // provider with the same generation options `decompose`
        // uses. To avoid duplicating the prompt construction, this
        // method rebuilds a minimal prompt: the same goal, the same
        // repo block, and the hint.
        //
        // The pragmatic trade: a re-plan that produces a *worse*
        // prompt than the first pass is worse than keeping the
        // original plan. The caller checks for an empty or
        // malformed reply and keeps the original on failure.
        let repo_context = self.probe_repo(goal).await;
        let repo_block = match &repo_context {
            Some(s) if !s.is_empty() => format!(
                "\nWorking directory context:\n{}\n\nReference the \
                 concrete files above where a subtask touches them, \
                 instead of naming files you have not seen.\n",
                s
            ),
            _ => String::new(),
        };
        let prompt = format!(
            "You are decomposing a task for a team of AI agents.\n\n\
             Goal: {goal}\n{repo}\
             \nYour previous attempt had a problem:\n\n{hint}\n\n\
             Produce a new decomposition with the same JSON shape: \
             \"name\", \"description\", \"capability\", \
             \"expected_writes\". This time, no two subtasks may \
             share a path prefix in their write sets.\n",
            goal = goal,
            repo = repo_block,
            hint = hint,
        );
        let text = self
            .provider
            .generate(
                &prompt,
                &GenerationOptions {
                    temperature: Some(0.2),
                    ..Default::default()
                },
            )
            .await?;
        let mut subs = parse_subtasks(&text, self.max_agents).unwrap_or_default();
        self.refine_subtask_metadata(&mut subs).await;
        Ok(subs)
    }

    /// Ask the model to split `goal` into at most `max_agents` subtasks.
    /// Falls back to N angle-hinted copies of the goal when the reply is
    /// not the requested JSON — a malformed reply should still produce a
    /// swarm, not a single agent.
    async fn decompose(&self, goal: &str) -> Result<Vec<Subtask>> {
        // Probe the working directory so the split is informed by
        // what is actually there: a repository context line listing
        // the top-level entries plus, when the goal's vocabulary
        // appears in the tree, a short list of files per keyword. The
        // decompose prompt is asked to reference these concrete paths
        // rather than invent them.
        let repo_context = self.probe_repo(goal).await;
        let repo_block = match &repo_context {
            Some(s) if !s.is_empty() => format!(
                "\nWorking directory context:\n{}\n\nReference the \
                 concrete files above where a subtask touches them, \
                 instead of naming files you have not seen.\n",
                s
            ),
            _ => String::new(),
        };

        let prompt = format!(
            "You are decomposing a task for a team of AI agents.\n\n\
             Goal: {goal}\n{repo}\
             \nSplit this goal into at most {n} independent subtasks that \
             can be worked on in parallel. Each subtask must be \
             self-contained: an agent receiving only its description, \
             plus the ability to read and write files and run shell \
             commands, must be able to complete it without seeing the \
             others.\n\n\
             Reply with a single JSON array and nothing else. Each element \
             has these fields:\n\
             - \"name\": short, kebab-case, 2-4 words.\n\
             - \"description\": one or two sentences, imperative.\n\
             - \"capability\": one of coding, testing, documentation, \
               code-review, planning, research, debugging, refactoring. \
               Pick the one that best describes the subtask's deliverable.\n\
             - \"expected_writes\": an array of path globs the subtask will \
               write to. Use specific paths (`src/parser.rs`) or narrow \
               prefixes (`src/parser/**`). Do NOT use `**` or `*` alone — \
               that tells the coordinator nothing and will force the whole \
               swarm to serialize. If a subtask genuinely does not write \
               files (research, review, planning), return an empty array.\n\
             - \"depends_on\": an array of \"name\" values of other subtasks \
               in this same list that must finish before this one starts. \
               Use it only when the subtask genuinely needs the other's \
               output — for example, an implementation subtask that \
               consumes a schema another subtask writes. Most subtasks \
               should have an empty array so the swarm parallelizes. Do \
               not create cycles (A depends on B, B depends on A): the \
               coordinator will run them together with a warning, and any \
               claims they share will be enforced at write time.\n\
             \n\
             Two subtasks may not share a prefix in their write sets. If \
             you find yourself wanting to write the same file from two \
             subtasks, either split the file's contents along a different \
             axis, or merge the subtasks.\n\
             \n\
             Example:\n\
             [{{\"name\":\"design-schema\",\"description\":\"Write the SQL \
             schema for a users table with id, email, created_at.\", \
             \"capability\":\"coding\", \
             \"expected_writes\":[\"migrations/001_users.sql\"], \
             \"depends_on\":[]}}]\n",
            goal = goal,
            repo = repo_block,
            n = self.max_agents,
        );

        let text = self
            .provider
            .generate(
                &prompt,
                &GenerationOptions {
                    temperature: Some(0.2),
                    ..Default::default()
                },
            )
            .await?;

        if let Some(v) = parse_subtasks(&text, self.max_agents)
            && !v.is_empty()
        {
            return Ok(v);
        }

        // Fallback: the model ignored the JSON instruction, or the
        // provider errored in a way that produced text we cannot parse.
        // N angle-hinted copies of the goal is not a clever split, but
        // it is an honest swarm.
        Ok((0..self.max_agents)
            .map(|i| Subtask {
                depends_on: Vec::new(),
                name: format!("angle-{}", i + 1),
                description: format!(
                    "{goal}\n\nFocus on a distinct angle from the other \
                     agents: agent {i} of {n}. Explore a different \
                     approach, do concrete work, and report what you \
                     found — the results will be merged.",
                    goal = goal,
                    i = i + 1,
                    n = self.max_agents,
                ),
                // No claim: the fallback path is what runs when
                // the planner could not be trusted to produce a
                // JSON array, so trusting it to produce a write
                // set would be worse. Empty means the merge-time
                // conflict detector is the only defence.
                expected_writes: Vec::new(),
                // Infer from the goal rather than the (identical)
                // description — the description here is a
                // permutation of the goal, not a subtask-specific
                // hint.
                capability: capability_for(goal),
            })
            .collect())
    }

    /// Ask the model to synthesize the per-agent results.
    /// Ask the model to synthesize the per-agent results.
    ///
    /// When `conflicts` is non-empty, the prompt leads with a warning
    /// naming the files and their authors, and instructs the model to
    /// reconcile. It can then say "agents A and B both edited
    /// `schema.sql`; the merged version is A's schema plus B's index"
    /// instead of ignoring the overlap.
    async fn merge(
        &self,
        goal: &str,
        per_agent: &[AgentResult],
        conflicts: &[FileConflict],
    ) -> Result<String> {
        let mut blocks = String::new();
        for r in per_agent {
            match &r.outcome {
                AgentOutcome::Completed(text) => blocks.push_str(&format!(
                    "\n### Agent: {}\nSubtask: {}\nResult:\n{}\n",
                    r.name, r.subtask, text
                )),
                AgentOutcome::Failed(e) => blocks.push_str(&format!(
                    "\n### Agent: {} (FAILED)\nSubtask: {}\nError: {}\n",
                    r.name, r.subtask, e
                )),
            }
        }

        let conflict_block = if conflicts.is_empty() {
            String::new()
        } else {
            let mut s = String::from(
                "Warning: multiple agents edited the same files. The merged \
                 answer must reconcile their changes and say which version \
                 (or combination) wins, naming the file:\n",
            );
            for c in conflicts {
                s.push_str(&format!(
                    "  - {} was written by {}\n",
                    c.file,
                    c.agents.join(", ")
                ));
            }
            s.push('\n');
            s
        };

        let prompt = format!(
            "You ran a team of {n} agents on this goal:\n\n{goal}\n\n\
             {conflicts}Each agent reported:\n{blocks}\n\
             Synthesize their work into a single coherent answer for the \
             user. Note any conflicts between agents. If an agent failed, \
             say what is missing. Do not repeat the per-agent blocks \
             verbatim — write the merged answer.\n",
            n = per_agent.len(),
            goal = goal,
            conflicts = conflict_block,
            blocks = blocks,
        );

        self.provider
            .generate(
                &prompt,
                &GenerationOptions {
                    temperature: Some(0.3),
                    ..Default::default()
                },
            )
            .await
    }

    fn concatenate(per_agent: &[AgentResult]) -> String {
        let mut out = String::new();
        for r in per_agent {
            out.push_str(&format!("=== {} ===\n", r.name));
            out.push_str(&format!("Subtask: {}\n\n", r.subtask));
            match &r.outcome {
                AgentOutcome::Completed(text) => out.push_str(text),
                AgentOutcome::Failed(e) => out.push_str(&format!("(failed: {e})")),
            }
            out.push_str("\n\n");
        }
        out.trim_end().to_string()
    }
}

/// Extract the canonical path of every file the agent wrote to.
///
/// Pairs `write_file` calls with their results (the tool resolves the
/// path and returns the canonical form) so two agents writing
/// `"shared.txt"` and `"./shared.txt"` are seen as touching the same
/// file. Falls back to the raw path argument when a result is missing
/// or not a `Success` — an agent whose write failed still "touched"
/// the file as far as conflict detection cares.
fn collect_writes(resp: &crate::router::TaskResponse) -> Vec<String> {
    use kod_types::ToolResult;
    let mut out = Vec::new();
    for (call, result) in resp.tool_calls.iter().zip(resp.tool_results.iter()) {
        if call.tool_name != "write_file" {
            continue;
        }
        let from_result = match result {
            ToolResult::Success(v) => v.get("path").and_then(|p| p.as_str()),
            _ => None,
        };
        let from_call = call.arguments.get("path").and_then(|p| p.as_str());
        if let Some(p) = from_result.or(from_call) {
            out.push(p.to_string());
        }
    }
    // A call with no paired result (the loop stopped at MAX_TOOL_ROUNDS
    // mid-round) still counts — the write may or may not have happened,
    // but the agent intended it.
    for call in &resp.tool_calls[resp.tool_results.len().min(resp.tool_calls.len())..] {
        if call.tool_name == "write_file"
            && let Some(p) = call.arguments.get("path").and_then(|p| p.as_str())
        {
            out.push(p.to_string());
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Per-role preamble prepended to a subtask description. Each role
/// gets one job; the preamble says what that job is. A writer that
/// tries to also test, or a tester that tries to fix, produces worse
/// output than an agent that stays in its lane.
fn role_preamble(cap: Capability) -> &'static str {
    match cap {
        Capability::Coding => {
            "You are the WRITER on this subtask. Produce the code (or \
             configuration) the subtask calls for. Write complete, runnable \
             files. Do not write tests for your own work — a separate tester \
             will. Do not run the test suite yourself — that is the tester's \
             job. Focus on getting the implementation right.\n\n"
        }
        Capability::Testing => {
            "You are the TESTER on this subtask. Write or run tests for the \
             code the subtask describes. Do not fix the code under test — if \
             a test fails, report the failure. Do not implement missing \
             functionality — write the test that would catch its absence and \
             report.\n\n"
        }
        Capability::Documentation => {
            "You are the DOCUMENTER on this subtask. Write user-facing \
             documentation — README sections, doc comments, guides — for the \
             code the subtask describes. Do not change the code.\n\n"
        }
        Capability::CodeReview => {
            "You are the REVIEWER on this subtask. Read the code the subtask \
             names and report findings: correctness bugs, security issues, \
             unclear structure, missing error handling. Do not fix what you \
             find — that is a writer's job. Include file and line references.\n\n"
        }
        Capability::Planning => {
            "You are the PLANNER on this subtask. Produce a concrete plan: \
             the files to touch, the order to touch them in, the interfaces \
             between them. Do not write the code — the writers will.\n\n"
        }
        Capability::Research => {
            "You are the RESEARCHER on this subtask. Gather facts and report \
             them: what exists, what the current state is, what the \
             constraints are. Do not write or modify code.\n\n"
        }
        Capability::Debugging => {
            "You are the DEBUGGER on this subtask. Find the root cause of the \
             failure the subtask describes. Reproduce it, isolate it, explain \
             it. A fix is optional — the diagnosis is the deliverable.\n\n"
        }
        Capability::Refactoring => {
            "You are the REFACTORER on this subtask. Change the structure of \
             the code the subtask names without changing its behavior. Do not \
             add features. Do not fix bugs you happen to notice — note them \
             instead.\n\n"
        }
    }
}

/// Reduce an arbitrary string to a short kebab-case token for an agent
/// name. Long names would make the labels hard to read in a stream.
fn sanitize(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("-")
        .chars()
        .take(24)
        .collect()
}

/// Detect a pairwise overlap between two subtasks' write claims.
///
/// Returns the indices of the first pair whose claims can touch the
/// same file, and the globs that produce the intersection — enough
/// for a diagnostic that names the offending paths.
///
/// The check is **conservative**: a false positive (two globs that
/// look like they might overlap but in practice would not) triggers
/// one re-plan, which is cheap; a false negative would let two
/// agents race on the same file, which is the entire failure mode
/// this function exists to prevent. The comparison over-approximates
/// by design.
///
/// The algorithm:
///
/// 1. `**` matches anything under the current directory and any
///    descendant — treat it as "claims the whole tree" and short-
///    circuit to a positive.
/// 2. Two paths overlap if one is a path-component prefix of the
///    other (`src/` vs `src/parser.rs`, `src/a` vs `src/a/b.rs`).
///    A wildcard segment inside either glob is truncated at the
///    first wildcard, and the resulting prefixes are compared as
///    above. This is what makes `src/parser/**` and `src/parser.rs`
///    overlap without comparing wildcard semantics.
/// 3. Two identical globs overlap trivially.
///
/// A claim that is an empty list never overlaps anything — the
/// planner has said "this subtask will not write files".
fn detect_overlap(subtasks: &[Subtask]) -> Option<(usize, usize, Vec<String>)> {
    for i in 0..subtasks.len() {
        for j in (i + 1)..subtasks.len() {
            let mut common: Vec<String> = Vec::new();
            for a in &subtasks[i].expected_writes {
                for b in &subtasks[j].expected_writes {
                    if globs_overlap(a, b) {
                        // Record both spellings so the diagnostic
                        // can show the two authors what collided.
                        common.push(format!("{a} ∩ {b}"));
                    }
                }
            }
            if !common.is_empty() {
                return Some((i, j, common));
            }
        }
    }
    None
}

/// Does either glob imply the other, at a path-component level?
fn globs_overlap(a: &str, b: &str) -> bool {
    // A `**` anywhere in either glob means the subtask claims the
    // whole tree under the current prefix. That is exactly the
    // pattern the decompose prompt tells the model not to produce,
    // and treating it as "overlaps everything" is what makes the
    // prompt's advice load-bearing.
    if a.contains("**") || b.contains("**") {
        return true;
    }
    let na = normalize_glob(a);
    let nb = normalize_glob(b);
    if na.is_empty() || nb.is_empty() {
        return false;
    }
    if na == nb {
        return true;
    }
    // Directory-prefix: `src/parser` vs `src/parser/foo.rs` (the
    // shorter names a directory, the longer names a file inside it).
    if na.starts_with(&format!("{nb}/")) || nb.starts_with(&format!("{na}/")) {
        return true;
    }
    // File-name-prefix in the same directory: `src/parser` and
    // `src/parser.rs` refer to the same logical file (one names the
    // stem, the other names the file). The test
    // `globs_overlap_catches_prefix_relations` pins this behaviour.
    let (da, fa) = match na.rfind('/') {
        Some(i) => (na[..i].to_string(), na[i + 1..].to_string()),
        None => (String::new(), na.clone()),
    };
    let (db, fb) = match nb.rfind('/') {
        Some(i) => (nb[..i].to_string(), nb[i + 1..].to_string()),
        None => (String::new(), nb.clone()),
    };
    da == db && (fa.starts_with(&fb) || fb.starts_with(&fa))
}

/// Truncate a glob at its first wildcard segment. `src/parser/*.rs`
/// becomes `src/parser`; `src/*/tests` becomes `src`; `README.md`
/// stays `README.md`.
///
/// A wildcard at the very start (`*.rs`) truncates to the empty
/// string, which `globs_overlap` treats as "no claim" — correct for
/// the case where the planner wrote a pattern that names no
/// directory.
fn normalize_glob(glob: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for seg in glob.trim_start_matches("./").split('/') {
        if seg.contains('*') || seg.contains('?') || seg.contains('[') {
            break;
        }
        if seg.is_empty() {
            continue;
        }
        parts.push(seg);
    }
    parts.join("/")
}

/// Best-effort capability from the subtask description. The coordinator
/// uses capabilities for its "find the right agent" queries; the runner
/// currently spawns one agent per subtask, so this is metadata rather
/// than dispatch policy.
fn capability_for(description: &str) -> Capability {
    let l = description.to_lowercase();
    if l.contains("test") {
        Capability::Testing
    } else if l.contains("document") || l.contains("readme") {
        Capability::Documentation
    } else if l.contains("review") {
        Capability::CodeReview
    } else if l.contains("design") || l.contains("plan") {
        Capability::Planning
    } else if l.contains("research") || l.contains("investigate") {
        Capability::Research
    } else if l.contains("debug") || l.contains("fix") {
        Capability::Debugging
    } else if l.contains("refactor") {
        Capability::Refactoring
    } else {
        Capability::Coding
    }
}

/// Method added to `SwarmRunner` via the inherent impl below: run
/// Jev's capability classifier and globs validation on each parsed
/// subtask (P4.6). A subtask whose capability the planner left
/// ambiguous gets the classifier's verdict; a subtask whose globs do
/// not match its description is logged for the caller to act on.
impl SwarmRunner {
    async fn refine_subtask_metadata(&self, subtasks: &mut [Subtask]) {
        for st in subtasks.iter_mut() {
            // Capability: Jev's classification wins over the
            // heuristic when it has an answer.
            if let Some(label) = self
                .engine
                .validate_subtask_capability(&st.description)
                .await
                && let Ok(c) = label.parse::<kod_swarm::Capability>()
            {
                st.capability = c;
            }
            // Globs: log a warning when they do not plausibly match
            // the description. The runner still proceeds — the
            // merge-time conflict detector is the backstop.
            if let Some(false) = self
                .engine
                .validate_subtask_globs(&st.description, &st.expected_writes)
                .await
            {
                tracing::warn!(
                    subtask = %st.name,
                    "Jev flagged subtask globs as not matching the description",
                );
            }
        }
    }
}

/// Extract a JSON array of `{name, description}` objects from a model
/// reply that may carry prose around it. Finds the first `[` and the
/// matching `]` and parses that slice.
fn parse_subtasks(text: &str, max: usize) -> Option<Vec<Subtask>> {
    let start = text.find('[')?;
    let end = text.rfind(']')?;
    if end <= start {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(&text[start..=end]).ok()?;
    let arr = v.as_array()?;
    let mut out = Vec::new();
    for item in arr {
        let name = item.get("name")?.as_str()?.to_string();
        let description = item.get("description")?.as_str()?.to_string();
        if description.trim().is_empty() {
            continue;
        }
        // `expected_writes` is a required field of the prompt's
        // JSON shape; a malformed value (a string instead of an
        // array, an array with non-string entries) degrades to an
        // empty list. An empty list means "no claim" — the merge-
        // time conflict detector is the fallback, so a bad value
        // here is a warning, not a failure.
        let expected_writes: Vec<String> = item
            .get("expected_writes")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str())
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        // `capability` is a hint; a missing or unknown value falls
        // back to the description-based inference the runner used
        // before this field existed. That inference is not removed
        // — it stays as the fallback path.
        let capability = item
            .get("capability")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<Capability>().ok())
            .unwrap_or_else(|| capability_for(&description));
        out.push(Subtask {
            name,
            description,
            expected_writes,
            capability,
            // `depends_on` is a list of subtask names. A missing or
            // malformed value degrades to an empty list — the
            // scheduler then runs the subtask in the first wave.
            depends_on: item
                .get("depends_on")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str())
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect()
                })
                .unwrap_or_default(),
        });
        if out.len() >= max {
            break;
        }
    }
    Some(out)
}



#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_role_preamble_distinct_per_role() {
        let caps = [
            Capability::Coding,
            Capability::Testing,
            Capability::Documentation,
            Capability::CodeReview,
            Capability::Planning,
            Capability::Research,
            Capability::Debugging,
            Capability::Refactoring,
        ];
        let mut seen: Vec<&str> = Vec::new();
        for cap in caps {
            let p = role_preamble(cap);
            assert!(!p.is_empty(), "{:?} has an empty preamble", cap);
            assert!(!seen.contains(&p), "{:?} shares its preamble", cap);
            seen.push(p);
        }
    }

    #[test]
    fn test_role_preamble_names_the_role() {
        assert!(role_preamble(Capability::Coding).contains("WRITER"));
        assert!(role_preamble(Capability::Testing).contains("TESTER"));
        assert!(role_preamble(Capability::Documentation).contains("DOCUMENTER"));
        assert!(role_preamble(Capability::CodeReview).contains("REVIEWER"));
    }

    #[test]
    fn sanitize_is_kebab_and_bounded() {
        assert_eq!(sanitize("Design Schema!"), "design-schema");
        assert_eq!(sanitize("a b  c"), "a-b-c");
        let long = "x".repeat(100);
        assert!(sanitize(&long).len() <= 24);
    }

    #[test]
    fn capability_inference_from_description() {
        assert_eq!(capability_for("write tests for auth"), Capability::Testing);
        assert_eq!(
            capability_for("update the readme"),
            Capability::Documentation
        );
        assert_eq!(
            capability_for("refactor the parser"),
            Capability::Refactoring
        );
        assert_eq!(capability_for("implement the handler"), Capability::Coding);
    }

    #[test]
    fn parse_subtasks_reads_json_with_surrounding_prose() {
        let text = "Here you go:\n[{\"name\":\"a\",\"description\":\"do a\"},\
                    {\"name\":\"b\",\"description\":\"do b\"}]\nDone.";
        let v = parse_subtasks(text, 10).unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].name, "a");
        assert_eq!(v[1].description, "do b");
    }

    #[test]
    fn parse_subtasks_clamps_to_max() {
        let text = "[{\"name\":\"a\",\"description\":\"a\"},\
                    {\"name\":\"b\",\"description\":\"b\"},\
                    {\"name\":\"c\",\"description\":\"c\"}]";
        assert_eq!(parse_subtasks(text, 2).unwrap().len(), 2);
    }

    #[test]
    fn globs_overlap_catches_prefix_relations() {
        // Identical globs.
        assert!(globs_overlap("src/parser.rs", "src/parser.rs"));
        // One is a component-prefix of the other.
        assert!(globs_overlap("src/parser.rs", "src/parser"));
        assert!(globs_overlap("src/parser", "src/parser.rs"));
        assert!(globs_overlap("src/a/b.rs", "src/a/b.rs"));
        // A `**` anywhere overlaps everything.
        assert!(globs_overlap("src/**", "src/parser.rs"));
        assert!(globs_overlap("docs/x.md", "**"));
        // Unrelated prefixes do not overlap.
        assert!(!globs_overlap("src/parser.rs", "src/http.rs"));
        assert!(!globs_overlap("src/a/b.rs", "src/c/d.rs"));
        // A wildcard segment truncates to its prefix.
        assert!(globs_overlap("src/parser/*.rs", "src/parser.rs"));
        assert!(globs_overlap("src/*/lib.rs", "src/api/lib.rs"));
    }

    #[test]
    fn detect_overlap_finds_first_colliding_pair() {
        let clean = vec![
            Subtask {
                name: "a".to_string(),
                description: "a".to_string(),
                expected_writes: vec!["src/parser.rs".to_string()],
                capability: Capability::Coding,
                depends_on: Vec::new(),
            },
            Subtask {
                name: "b".to_string(),
                description: "b".to_string(),
                expected_writes: vec!["src/http.rs".to_string()],
                capability: Capability::Coding,
                depends_on: Vec::new(),
            },
        ];
        assert!(detect_overlap(&clean).is_none());

        let dirty = vec![
            Subtask {
                name: "a".to_string(),
                description: "a".to_string(),
                expected_writes: vec!["src/parser.rs".to_string()],
                capability: Capability::Coding,
                depends_on: Vec::new(),
            },
            Subtask {
                name: "b".to_string(),
                description: "b".to_string(),
                expected_writes: vec!["src/parser.rs".to_string()],
                capability: Capability::Coding,
                depends_on: Vec::new(),
            },
        ];
        let (i, j, common) = detect_overlap(&dirty).expect("collision");
        assert_eq!((i, j), (0, 1));
        assert!(!common.is_empty());
    }

    #[test]
    fn empty_write_set_never_overlaps() {
        let subtasks = vec![
            Subtask {
                name: "a".to_string(),
                description: "a".to_string(),
                expected_writes: Vec::new(),
                capability: Capability::Research,
                depends_on: Vec::new(),
            },
            Subtask {
                name: "b".to_string(),
                description: "b".to_string(),
                expected_writes: Vec::new(),
                capability: Capability::Planning,
                depends_on: Vec::new(),
            },
        ];
        assert!(detect_overlap(&subtasks).is_none());
    }

    #[test]
    fn parse_subtasks_reads_the_new_fields() {
        let text = r#"[{"name":"n","description":"d","capability":"testing","expected_writes":["src/a.rs","docs/**"]}]"#;
        let v = parse_subtasks(text, 5).expect("parse");
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].capability, Capability::Testing);
        assert_eq!(
            v[0].expected_writes,
            vec!["src/a.rs".to_string(), "docs/**".to_string()]
        );
    }

    #[test]
    fn parse_subtasks_degrades_missing_fields_gracefully() {
        // Capability omitted: falls back to description inference.
        // expected_writes omitted: empty list.
        let text = r#"[{"name":"n","description":"write tests for auth"}]"#;
        let v = parse_subtasks(text, 5).expect("parse");
        assert_eq!(v[0].capability, Capability::Testing);
        assert!(v[0].expected_writes.is_empty());
    }

    #[test]
    fn parse_subtasks_returns_none_on_garbage() {
        assert!(parse_subtasks("no json here", 5).is_none());
        assert!(parse_subtasks("[not json]", 5).is_none());
        assert!(parse_subtasks("[]", 5).unwrap().is_empty());
    }
}

#[cfg(test)]
mod coverage_glob_overlap {
    //! `globs_overlap` is the first line of defence against two
    //! swarm agents racing on the same file. The function is
    //! deliberately conservative: a false positive triggers one
    //! re-plan (cheap), a false negative lets two agents write
    //! the same file (the failure mode the design calls out).
    //! These pin both the cases it must catch and the cases it
    //! must not.
    use super::*;

    #[test]
    fn identical_globs_overlap() {
        assert!(globs_overlap("src/a.rs", "src/a.rs"));
        assert!(globs_overlap("docs/**", "docs/**"));
    }

    #[test]
    fn directory_prefix_overlaps_with_file_inside_it() {
        assert!(globs_overlap("src", "src/a.rs"));
        assert!(globs_overlap("src/a.rs", "src"));
        assert!(globs_overlap("src/a", "src/a/b.rs"));
    }

    #[test]
    fn stem_and_extension_collide() {
        // `src/a` and `src/a.rs` name the same logical file: one
        // gives the stem, the other gives the file. The design
        // treats these as overlapping so the planner re-plans
        // rather than letting two agents race on the same path.
        assert!(globs_overlap("src/a", "src/a.rs"));
        assert!(globs_overlap("src/parser", "src/parser.rs"));
    }

    #[test]
    fn double_star_overlaps_with_everything() {
        // `**` is the whole-tree glob. Treating it as "overlaps
        // everything" is what makes the decompose prompt's "do
        // not use `**` alone" advice load-bearing.
        assert!(globs_overlap("**", "src/a.rs"));
        assert!(globs_overlap("src/**", "docs/readme.md"));
        assert!(globs_overlap("a/b/**", "x/y/z"));
    }

    #[test]
    fn unrelated_prefixes_do_not_overlap() {
        assert!(!globs_overlap("src/parser.rs", "src/http.rs"));
        assert!(!globs_overlap("docs/a.md", "tests/b.rs"));
        assert!(!globs_overlap("src/a/b.rs", "src/c/d.rs"));
    }

    #[test]
    fn wildcard_segment_truncates_to_its_prefix() {
        // `src/parser/*.rs` truncates to `src/parser`; a file at
        // that prefix overlaps.
        assert!(globs_overlap("src/parser/*.rs", "src/parser.rs"));
        assert!(globs_overlap("src/parser/*.rs", "src/parser/foo.rs"));
        // And does not reach a sibling prefix.
        assert!(!globs_overlap("src/parser/*.rs", "src/http/x.rs"));
    }

    #[test]
    fn wildcard_at_the_start_truncates_to_empty() {
        // `*.rs` has no directory segment before the wildcard.
        // `normalize_glob` truncates to the empty string, and the
        // overlap check treats that as "no claim". The behaviour
        // is documented rather than hidden.
        assert!(!globs_overlap("*.rs", "src/a.rs"));
    }

    #[test]
    fn leading_dot_slash_is_stripped() {
        // `./src/a.rs` and `src/a.rs` name the same path. The
        // normalizer strips the leading `./` so the comparison is
        // on the canonical form.
        assert!(globs_overlap("./src/a.rs", "src/a.rs"));
    }

    #[test]
    fn detect_overlap_finds_the_first_pair() {
        let subtasks = vec![
            Subtask {
                name: "a".into(),
                description: "a".into(),
                expected_writes: vec!["src/parser.rs".into()],
                capability: Capability::Coding,
                depends_on: vec![],
            },
            Subtask {
                name: "b".into(),
                description: "b".into(),
                expected_writes: vec!["src/http.rs".into()],
                capability: Capability::Coding,
                depends_on: vec![],
            },
            Subtask {
                name: "c".into(),
                description: "c".into(),
                expected_writes: vec!["src/parser.rs".into()],
                capability: Capability::Coding,
                depends_on: vec![],
            },
        ];
        let hit = detect_overlap(&subtasks);
        let (i, j, common) = hit.expect("expected a collision");
        assert_eq!((i, j), (0, 2));
        assert!(!common.is_empty());
    }

    #[test]
    fn detect_overlap_returns_none_for_a_clean_plan() {
        let subtasks = vec![
            Subtask {
                name: "a".into(),
                description: "a".into(),
                expected_writes: vec!["src/a.rs".into()],
                capability: Capability::Coding,
                depends_on: vec![],
            },
            Subtask {
                name: "b".into(),
                description: "b".into(),
                expected_writes: vec!["docs/b.md".into()],
                capability: Capability::Documentation,
                depends_on: vec![],
            },
        ];
        assert!(detect_overlap(&subtasks).is_none());
    }

    #[test]
    fn empty_write_sets_never_overlap() {
        let subtasks = vec![
            Subtask {
                name: "a".into(),
                description: "a".into(),
                expected_writes: vec![],
                capability: Capability::Research,
                depends_on: vec![],
            },
            Subtask {
                name: "b".into(),
                description: "b".into(),
                expected_writes: vec![],
                capability: Capability::Planning,
                depends_on: vec![],
            },
        ];
        assert!(detect_overlap(&subtasks).is_none());
    }
}
