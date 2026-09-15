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
#[derive(Debug, Clone)]
pub struct Subtask {
    pub name: String,
    pub description: String,
}

/// Progress events a swarm run emits while it works. The consumer
/// decides what to render; the runner does not print.
#[derive(Debug, Clone)]
pub enum SwarmEvent {
    /// The decompose step produced these subtasks.
    Decomposed(Vec<Subtask>),
    /// Agent started on its subtask.
    AgentStarted {
        id: AgentId,
        name: String,
        subtask: String,
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
    /// All agents done; the runner is now calling the model to merge.
    Merging,
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
    /// The merged answer.
    pub merged: String,
    /// True when `merged` came from a synthesis call; false when it is a
    /// concatenation (config disabled, or the synthesis call failed).
    pub merged_by_model: bool,
}

/// Runs a swarm. Construct once per goal; `run` is the only entry point.
pub struct SwarmRunner {
    engine: Arc<KodEngine>,
    provider: Arc<dyn LlmProvider>,
    max_agents: usize,
    merge_results: bool,
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
        let provider = engine.provider_arc().await.ok_or_else(|| {
            KodError::InvalidState(
                "SwarmRunner: no LLM provider installed. \
                 Call engine.set_provider(...) first."
                    .to_string(),
            )
        })?;
        Ok(Self {
            engine,
            provider,
            max_agents: max_agents.clamp(2, 8),
            merge_results,
        })
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
        // 1. Decompose.
        let subtasks = self.decompose(goal).await?;
        let _ = chunk_tx.send(SwarmEvent::Decomposed(subtasks.clone())).await;

        // 2. Spawn agents and register tasks.
        let working_dir = self.engine.working_dir().to_path_buf();
        let swarm = AgentSwarm::new(working_dir);
        let mut handles: Vec<AgentHandle> = Vec::new();
        for (i, st) in subtasks.iter().enumerate() {
            let name = format!("agent-{}-{}", i + 1, sanitize(&st.name));
            let agent = AgentBuilder::new(&name)
                .with_capability(capability_for(&st.description))
                .build();
            let id = agent.id().clone();
            swarm.add_agent(agent).await?;
            swarm.start_agent(&id).await?;

            let task = Task::new(st.description.clone(), Priority::Medium);
            let task_id = task.id.clone();
            swarm.coordinator().register_task(task).await?;
            swarm.coordinator().assign_task(&task_id, &id).await?;

            handles.push(AgentHandle {
                id,
                name,
                subtask: st.clone(),
                task_id,
            });
        }

        // 3. Run all agents concurrently. Each gets its own channel so
        //    the engine streams per-agent text; a small forwarding task
        //    relabels tool markers and pushes onto the outgoing channel.
        let mut tasks = Vec::with_capacity(handles.len());
        for h in &handles {
            let engine = self.engine.clone();
            let id = h.id.clone();
            let name = h.name.clone();
            let subtask = h.subtask.clone();
            let out = chunk_tx.clone();
            tasks.push(async move {
                let _ = out
                    .send(SwarmEvent::AgentStarted {
                        id: id.clone(),
                        name: name.clone(),
                        subtask: subtask.description.clone(),
                    })
                    .await;

                let (tx, mut rx) = mpsc::channel::<String>(64);
                let out_pump = out.clone();
                let id_pump = id.clone();
                let name_pump = name.clone();
                let pump = tokio::spawn(async move {
                    while let Some(chunk) = rx.recv().await {
                        // Turn the engine's control markers into a
                        // human-readable line. A swarm consumer wants to
                        // see that a tool ran, not the raw marker.
                        let display = if let Some(tool) =
                            crate::engine::parse_tool_start(&chunk)
                        {
                            format!("  [tool: {tool}]\n")
                        } else if let Some(brief) = crate::engine::parse_tool_args(&chunk) {
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

                let result = engine
                    .process_streaming(&subtask.description, &tx)
                    .await;
                drop(tx);
                let _ = pump.await;

                match result {
                    Ok(resp) => (id, name, subtask, Ok(resp.text.unwrap_or_default())),
                    Err(e) => (id, name, subtask, Err(e.to_string())),
                }
            });
        }

        let raw = join_all(tasks).await;

        // 4. Report terminal status. The coordinator's load accounting
        //    needs the completion, and the events let a live UI update.
        let mut per_agent = Vec::with_capacity(raw.len());
        for (id, name, subtask, outcome) in raw {
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

        // 5. Merge.
        let (merged, merged_by_model) = if self.merge_results {
            let _ = chunk_tx.send(SwarmEvent::Merging).await;
            match self.merge(goal, &per_agent).await {
                Ok(s) if !s.trim().is_empty() => (s, true),
                _ => (Self::concatenate(&per_agent), false),
            }
        } else {
            (Self::concatenate(&per_agent), false)
        };

        Ok(SwarmResponse {
            subtasks,
            per_agent,
            merged,
            merged_by_model,
        })
    }

    /// Ask the model to split `goal` into at most `max_agents` subtasks.
    /// Falls back to N angle-hinted copies of the goal when the reply is
    /// not the requested JSON — a malformed reply should still produce a
    /// swarm, not a single agent.
    async fn decompose(&self, goal: &str) -> Result<Vec<Subtask>> {
        let prompt = format!(
            "You are decomposing a task for a team of AI agents.\n\n\
             Goal: {goal}\n\n\
             Split this goal into at most {n} independent subtasks that \
             can be worked on in parallel. Each subtask must be \
             self-contained: an agent receiving only its description, \
             plus the ability to read and write files and run shell \
             commands, must be able to complete it without seeing the \
             others.\n\n\
             Reply with a single JSON array and nothing else. Each element \
             has a \"name\" (short, kebab-case, 2-4 words) and a \
             \"description\" (one or two sentences, imperative). Example:\n\
             [{{\"name\":\"design-schema\",\"description\":\"Write the SQL \
             schema for a users table with id, email, created_at.\"}}]\n",
            goal = goal,
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
            })
            .collect())
    }

    /// Ask the model to synthesize the per-agent results.
    async fn merge(&self, goal: &str, per_agent: &[AgentResult]) -> Result<String> {
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

        let prompt = format!(
            "You ran a team of {n} agents on this goal:\n\n{goal}\n\n\
             Each agent reported:\n{blocks}\n\
             Synthesize their work into a single coherent answer for the \
             user. Note any conflicts between agents. If an agent failed, \
             say what is missing. Do not repeat the per-agent blocks \
             verbatim — write the merged answer.\n",
            n = per_agent.len(),
            goal = goal,
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

struct AgentHandle {
    id: AgentId,
    name: String,
    subtask: Subtask,
    task_id: TaskId,
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
        out.push(Subtask { name, description });
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
    fn sanitize_is_kebab_and_bounded() {
        assert_eq!(sanitize("Design Schema!"), "design-schema");
        assert_eq!(sanitize("a b  c"), "a-b-c");
        let long = "x".repeat(100);
        assert!(sanitize(&long).len() <= 24);
    }

    #[test]
    fn capability_inference_from_description() {
        assert_eq!(capability_for("write tests for auth"), Capability::Testing);
        assert_eq!(capability_for("update the readme"), Capability::Documentation);
        assert_eq!(capability_for("refactor the parser"), Capability::Refactoring);
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
    fn parse_subtasks_returns_none_on_garbage() {
        assert!(parse_subtasks("no json here", 5).is_none());
        assert!(parse_subtasks("[not json]", 5).is_none());
        assert!(parse_subtasks("[]", 5).unwrap().is_empty());
    }
}
