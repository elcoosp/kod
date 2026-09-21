//! Swarm agent state on `KodApp`.
//!
//! Extracted from `app/mod.rs` (S9). Owns both the older
//! `agents: HashMap` used by the agent panel and the newer
//! `swarm_agents` / `swarm_agent_order` map driven by `SwarmEvent`s
//! from the runner. The two coexist because the panel renders both
//! views.

use super::*;

impl KodApp {
    pub fn agents(&self) -> Vec<&AgentInfo> {
        self.agents.values().collect()
    }

    pub fn agents_map(&self) -> &HashMap<String, AgentInfo> {
        &self.agents
    }

    pub fn add_agent(&mut self, name: &str, capabilities: Vec<String>) {
        self.agents.insert(
            name.to_string(),
            AgentInfo {
                name: name.to_string(),
                capabilities,
                status: "idle".to_string(),
                current_task: None,
            },
        );
    }

    pub fn get_agent(&self, name: &str) -> Option<&AgentInfo> {
        self.agents.get(name)
    }

    pub fn update_agent_status(&mut self, name: &str, status: &str) {
        if let Some(agent) = self.agents.get_mut(name) {
            agent.status = status.to_string();
        }
    }

    pub fn update_agent_task(&mut self, name: &str, task: &str) {
        if let Some(agent) = self.agents.get_mut(name) {
            agent.current_task = Some(task.to_string());
        }
    }

    /// The live swarm-agent views, keyed by id. Read by the agent
    /// panel (D4-D5) and any future status surface.
    pub fn swarm_agents(&self) -> &std::collections::HashMap<kod_types::AgentId, SwarmAgentView> {
        &self.swarm_agents
    }

    /// The agent id at 1-based position `n` in the current run's start
    /// order, or `None` when there is no such agent. Backs the `@N`
    /// focus syntax in the input box: `@2 do X` finds the second agent
    /// that began work this run and steers it.
    pub fn swarm_agent_by_index(&self, n: usize) -> Option<&kod_types::AgentId> {
        if n == 0 {
            return None;
        }
        self.swarm_agent_order.get(n - 1)
    }

    /// Prepare for a new swarm run: clears the live-agent map so a
    /// previous run's rows are not appended to.
    pub fn begin_swarm(&mut self) {
        self.swarm_agents.clear();
        self.swarm_agent_order.clear();
    }

    /// Announce the decompose results as a system line.
    pub fn swarm_decomposed(&mut self, subtasks: &[(String, String)]) {
        let mut s = format!("Swarm: {} subtasks\n", subtasks.len());
        for (i, (name, desc)) in subtasks.iter().enumerate() {
            let d = desc.lines().next().unwrap_or(desc);
            s.push_str(&format!("  {}. {} — {}\n", i + 1, name, d));
        }
        self.push_system_message(s.trim_end());
    }

    /// Create a chat row for a starting agent.
    pub fn swarm_agent_started(
        &mut self,
        id: kod_types::AgentId,
        name: &str,
        subtask: &str,
        model: Option<String>,
    ) {
        let header = format!("{name} — {}", subtask.lines().next().unwrap_or(subtask));
        let msg_id = MessageId::new();
        self.add_message(Message {
            id: msg_id.clone(),
            role: MessageRole::Agent(id.clone()),
            content: header,
            timestamp: Utc::now(),
            metadata: MessageMetadata::default(),
            sequence: 0,
        });
        self.swarm_agents.insert(
            id.clone(),
            SwarmAgentView {
                message_id: msg_id,
                finished: false,
                name: name.to_string(),
                subtask: subtask.lines().next().unwrap_or(subtask).to_string(),
                model,
                worktree: None,
                branch: None,
                tool_count: 0,
                failure: None,
                retry_note: None,
            },
        );
        self.swarm_agent_order.push(id);
    }

    /// Append a text chunk to a live agent's row.
    pub fn swarm_agent_chunk(&mut self, id: &kod_types::AgentId, text: &str) {
        // A chunk starting with "  [tool: " is a tool-start notice the
        // engine emits; its count is what the panel shows as
        // "N tools".
        let is_tool_marker = text.starts_with("  [tool: ");
        let msg_id = {
            let Some(view) = self.swarm_agents.get_mut(id) else {
                return;
            };
            if view.finished {
                return;
            }
            if is_tool_marker {
                view.tool_count += 1;
            }
            view.message_id.clone()
        };
        if let Some(msg) = self.messages.iter_mut().find(|m| m.id == msg_id) {
            msg.content.push_str(text);
        }
    }

    /// Replace the live row's trailing buffer with the agent's final
    /// result. The header stays.
    pub fn swarm_agent_finished(&mut self, id: &kod_types::AgentId, result: &str) {
        let Some(view) = self.swarm_agents.get_mut(id) else {
            return;
        };
        view.failure = None;
        view.retry_note = None;
        let msg_id = view.message_id.clone();
        if let Some(msg) = self.messages.iter_mut().find(|m| m.id == msg_id) {
            let header = msg
                .content
                .split_once('\n')
                .map(|(h, _)| h.to_string())
                .unwrap_or_else(|| msg.content.clone());
            let body = Self::trim_blank_lines(result);
            msg.content = if body.is_empty() {
                header
            } else {
                format!("{header}\n{body}")
            };
        }
        view.finished = true;
    }

    /// Attach worktree info to a live agent's view (D4-D5).
    pub fn swarm_set_worktree(
        &mut self,
        id: &kod_types::AgentId,
        path: std::path::PathBuf,
        branch: String,
    ) {
        if let Some(view) = self.swarm_agents.get_mut(id) {
            view.worktree = Some(path);
            view.branch = Some(branch);
        }
    }

    /// Record that an agent is retrying (D4-D5).
    pub fn swarm_set_retrying(
        &mut self,
        id: &kod_types::AgentId,
        attempt: u32,
        max_attempts: u32,
        previous_error: &str,
    ) {
        if let Some(view) = self.swarm_agents.get_mut(id) {
            view.retry_note = Some(format!(
                "retrying ({}/{}): {}",
                attempt,
                max_attempts,
                previous_error.lines().next().unwrap_or(""),
            ));
        }
    }

    /// Mark a live row as failed and replace its buffer with the error.
    pub fn swarm_agent_failed(&mut self, id: &kod_types::AgentId, error: &str) {
        let Some(view) = self.swarm_agents.get_mut(id) else {
            return;
        };
        view.failure = Some(error.to_string());
        let msg_id = view.message_id.clone();
        if let Some(msg) = self.messages.iter_mut().find(|m| m.id == msg_id) {
            let header = msg
                .content
                .split_once('\n')
                .map(|(h, _)| h.to_string())
                .unwrap_or_else(|| msg.content.clone());
            msg.content = format!("{header}\n(failed: {})", error.trim());
        }
        view.finished = true;
    }

    /// Finish the swarm: push the merged answer as an assistant row.
    pub fn swarm_complete(&mut self, merged: &str) {
        if !merged.trim().is_empty() {
            self.push_assistant_message(merged);
        }
        self.is_streaming = false;
        self.current_response.clear();
        self.generating = false;
        self.spinner_started = None;
        self.set_phase(GenPhase::Idle);
        self.fail_count = 0;
        self.scroll_to_bottom();
    }
}
