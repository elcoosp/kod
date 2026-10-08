//! Tool-execution rows on `KodApp`.
//!
//! Extracted from `app/mod.rs` (S9). Covers the lifecycle of a single
//! tool call's rendered row — start / update / complete / fail — plus
//! the expand/collapse state that decides whether its body is shown.
//!
//! Call-id keyed: the engine threads the provider's tool-call id through
//! every marker (protocol v2); this module registers each live row under
//! its call id so parallel calls fill *their own* rows. A call with no
//! provider id (`""`) falls back to the pre-v2 text heuristics.

use super::*;

impl KodApp {
    /// Bound on `tool_executions`. A long session can mint hundreds
    /// of thousands of tool rows; `/stats` only shows recent history,
    /// and the ledger's only other use is "is any entry still
    /// Running?", which only the recent tail can answer. 512 is
    /// several sessions' worth of recent calls.
    pub(crate) const TOOL_EXECUTIONS_CAP: usize = 512;

    /// Bound on `completed_calls`. A duplicate `ToolCompleted` /
    /// `ToolFailed` for the same id is only meaningful within the
    /// same engine round (a few hundred calls); 4096 is comfortably
    /// beyond that and keeps the set from growing without limit.
    pub(crate) const COMPLETED_CALLS_CAP: usize = 4096;

    /// Record a call id in `completed_calls`, evicting the oldest if
    /// the cap is reached. The order queue mirrors the set's
    /// insertion order; both are kept in sync.
    fn record_completed_call(&mut self, id: &str) {
        let id = id.to_string();
        if !self.completed_calls.insert(id.clone()) {
            return;
        }
        self.completed_calls_order.push_back(id);
        while self.completed_calls_order.len() > Self::COMPLETED_CALLS_CAP {
            if let Some(old) = self.completed_calls_order.pop_front() {
                self.completed_calls.remove(&old);
            }
        }
    }

    /// Push a tool-execution ledger entry, evicting the oldest if the
    /// cap is reached. A still-Running entry in the eviction window
    /// is settled first (`Failed` with a "evicted" note) so the
    /// running-tool counter stays accurate.
    fn push_tool_execution(&mut self, execution: ToolExecution) {
        if self.tool_executions.len() >= Self::TOOL_EXECUTIONS_CAP {
            // Find the oldest non-Running entry to evict. If every
            // entry is Running (pathological), evict the oldest
            // anyway after marking it settled so the counter is
            // honest.
            let evict_idx = self
                .tool_executions
                .iter()
                .position(|e| e.status != ToolStatus::Running)
                .unwrap_or(0);
            if self.tool_executions[evict_idx].status == ToolStatus::Running {
                self.tool_executions[evict_idx].status = ToolStatus::Failed;
                self.tool_executions[evict_idx].result =
                    Some("evicted from the execution ledger at the cap".to_string());
            }
            self.tool_executions.remove(evict_idx);
        }
        self.tool_executions.push(execution);
    }

    pub fn current_tool(&self) -> Option<&String> {
        self.current_tool.as_ref()
    }

    pub fn tool_executions(&self) -> &[ToolExecution] {
        &self.tool_executions
    }

    /// Delta §11.4: number of tool rows still showing the live "running…"
    /// placeholder. Used by the status strip to render an aggregate when
    /// more than one call is in flight.
    pub fn running_tool_count(&self) -> usize {
        self.messages
            .iter()
            .filter(|m| {
                m.role == MessageRole::Tool
                    && m.content.split_once('\n').map(|x| x.1).unwrap_or("").trim()
                        == Self::LIVE_TOOL_BODY_PLACEHOLDER
            })
            .count()
    }

    pub fn start_tool_execution(&mut self, id: &str, tool_name: &str) {
        // Repeat start for a call we already finished (duplicated marker,
        // engine round restart): never spawn a second row, and never
        // resurrect "running…" state for a settled call.
        if !id.is_empty() {
            if self.completed_calls.contains(id) {
                return;
            }
            if let Some(msg_id) = self.tool_rows_by_call.get(id).cloned() {
                self.refresh_tool_row_header(&msg_id, tool_name);
                self.current_tool = Some(tool_name.to_string());
                self.set_phase(GenPhase::ExecutingTool(tool_name.to_string()));
                return;
            }
        }

        self.current_tool = Some(tool_name.to_string());
        self.set_phase(GenPhase::ExecutingTool(tool_name.to_string()));

        self.push_tool_execution(ToolExecution {
            tool_name: tool_name.to_string(),
            status: ToolStatus::Running,
            start_time: Utc::now(),
            result: None,
        });
        // Stream the row live: it lands in position now with a running
        // body, and completion fills that same row in — the call never
        // arrives as a block at task end.
        let msg = Message {
            id: MessageId::new(),
            role: MessageRole::Tool,
            content: format!("[{tool_name}]\n{}", Self::LIVE_TOOL_BODY_PLACEHOLDER),
            timestamp: Utc::now(),
            metadata: MessageMetadata::default(),
            sequence: 0,
        };
        let msg_id = msg.id.clone();
        if !id.is_empty() {
            self.tool_rows_by_call.insert(id.to_string(), msg_id);
        }
        self.add_message(msg);
    }

    /// Rewrite one registered row's header in place, keeping its body.
    fn refresh_tool_row_header(&mut self, msg_id: &MessageId, header: &str) {
        if let Some(m) = self.messages.iter_mut().find(|m| m.id == *msg_id) {
            let body = m
                .content
                .split_once('\n')
                .map(|x| x.1.to_string())
                .unwrap_or_else(|| Self::LIVE_TOOL_BODY_PLACEHOLDER.to_string());
            m.content = format!("[{header}]\n{body}");
        }
    }

    /// Whether `m` (a tool row) matches `tool_name` by header text,
    /// ignoring an optional ` · 1.3s` duration stamp. Used by the
    /// empty-id legacy path: the engine may rewrite the header with a
    /// duration (from the live done-marker) and the task-end fallback
    /// then arrives with the bare name. Without the fuzzy match the
    /// fallback would mint a second row.
    fn tool_row_matches_header(m: &Message, tool_name: &str) -> bool {
        if m.role != MessageRole::Tool {
            return false;
        }
        let first = m.content.lines().next().unwrap_or("").trim();
        let header = first
            .strip_prefix('[')
            .and_then(|s| s.strip_suffix(']'))
            .unwrap_or(first);
        let base = header.split(" \u{00b7} ").next().unwrap_or(header);
        base == tool_name
            || base.starts_with(&format!("{tool_name} "))
            || tool_name.starts_with(&format!("{base} "))
            || base.contains(tool_name)
            || tool_name.contains(base)
    }

    /// Index of the most recent tool row still awaiting its result
    /// (legacy path for calls whose provider sends no ids).
    fn live_tool_msg(&self) -> Option<usize> {
        self.messages.iter().rposition(|m| {
            if m.role != MessageRole::Tool {
                return false;
            }
            let body = m.content.split_once('\n').map(|x| x.1).unwrap_or("").trim();
            body == Self::LIVE_TOOL_BODY_PLACEHOLDER
        })
    }

    /// Resolve the row a completion targets: the registered call row
    /// first, then the newest live placeholder, then (legacy, empty-id
    /// only) a header-prefix match.
    fn resolve_tool_row(&mut self, id: &str, tool_name: &str) -> Option<MessageId> {
        if !id.is_empty()
            && let Some(msg_id) = self.tool_rows_by_call.remove(id)
        {
            return Some(msg_id);
        }
        if let Some(i) = self.live_tool_msg() {
            return Some(self.messages[i].id.clone());
        }
        if id.is_empty()
            && let Some(i) = self.messages.iter().rposition(|m| {
                m.role == MessageRole::Tool && m.content.starts_with(&format!("[{tool_name}]"))
            })
        {
            return Some(self.messages[i].id.clone());
        }
        None
    }

    /// Refresh the live "running …" line with a one-line excerpt of what
    /// the tool is actually doing (`execute_command cargo test …`).
    /// Arrives from the engine after the call's arguments are assembled.
    pub fn update_tool_status(&mut self, id: &str, display: &str) {
        let display = display.trim();
        if display.is_empty() {
            return;
        }
        self.current_tool = Some(display.to_string());
        self.set_phase(GenPhase::ExecutingTool(display.to_string()));
        if let Some(msg_id) = self.tool_rows_by_call.get(id).cloned() {
            self.refresh_tool_row_header(&msg_id, display);
        } else if let Some(i) = self.live_tool_msg() {
            let body = self.messages[i]
                .content
                .split_once('\n')
                .map(|x| x.1)
                .unwrap_or(Self::LIVE_TOOL_BODY_PLACEHOLDER);
            let body = body.to_string();
            self.messages[i].content = format!("[{display}]\n{body}");
        }
    }

    pub fn complete_tool_execution(&mut self, id: &str, tool_name: &str, result: &str) {
        self.complete_tool_execution_with_duration(id, tool_name, result, None);
    }

    /// Fill the addressed tool row with its result, stamping the header
    /// with wall time when `duration_ms` is present (`header · 1.2s`).
    ///
    /// Idempotent by call id: the task-end fallback for a call the live
    /// done-marker already completed is a no-op.
    pub fn complete_tool_execution_with_duration(
        &mut self,
        id: &str,
        tool_name: &str,
        result: &str,
        duration_ms: Option<u64>,
    ) {
        if !id.is_empty() && self.completed_calls.contains(id) {
            return;
        }
        // Legacy (no provider id): the live done-marker already filled the
        // row with a duration and the task-end fallback has no id to
        // recognize it by. Match by header text instead — a completed row
        // for the same tool name is the idempotency we want.
        if id.is_empty()
            && let Some(i) = self
                .messages
                .iter()
                .rposition(|m| Self::tool_row_matches_header(m, tool_name))
        {
            let body = self.messages[i]
                .content
                .split_once('\n')
                .map(|x| x.1)
                .unwrap_or("")
                .trim();
            if !body.is_empty() && body != Self::LIVE_TOOL_BODY_PLACEHOLDER {
                return;
            }
        }

        // Update the ledger entry for the running call so `/stats` reflects
        // the completion.
        if let Some(execution) = self.tool_executions.iter_mut().rev().find(|e| {
            e.status == ToolStatus::Running
                && (e.tool_name == tool_name
                    || tool_name.starts_with(&format!("{} ", e.tool_name))
                    || tool_name.contains(&e.tool_name))
        }) {
            execution.status = ToolStatus::Completed;
            execution.result = Some(result.to_string());
        }

        let msg_id = self.resolve_tool_row(id, tool_name);
        self.current_tool = None;
        self.set_phase(GenPhase::Summarizing);

        let body = Self::trim_blank_lines(result);
        let is_error = body.trim_start().starts_with("Error:");
        let header = match duration_ms {
            Some(ms) => format!("{tool_name} · {}", kod_core::engine::format_duration_ms(ms)),
            None => tool_name.to_string(),
        };
        let content = format!("[{header}]\n{body}");

        let msg_id = match msg_id {
            Some(msg_id) => {
                if let Some(m) = self.messages.iter_mut().find(|m| m.id == msg_id) {
                    m.content = content;
                }
                msg_id
            }
            None => {
                let msg = Message {
                    id: MessageId::new(),
                    role: MessageRole::Tool,
                    content,
                    timestamp: Utc::now(),
                    metadata: MessageMetadata::default(),
                    sequence: 0,
                };
                let msg_id = msg.id.clone();
                self.add_message(msg);
                msg_id
            }
        };

        if !id.is_empty() {
            self.tool_rows_by_call.remove(id);
            self.record_completed_call(id);
        }
        // Errors must be unmistakable: auto-expand so the full message is
        // visible and never hidden behind the preview cap.
        if is_error {
            self.expanded_tools.insert(msg_id);
        }
    }

    pub fn fail_tool_execution(&mut self, id: &str, tool_name: &str, error: &str) {
        if !id.is_empty() && self.completed_calls.contains(id) {
            return;
        }
        if id.is_empty()
            && let Some(i) = self
                .messages
                .iter()
                .rposition(|m| Self::tool_row_matches_header(m, tool_name))
        {
            let body = self.messages[i]
                .content
                .split_once('\n')
                .map(|x| x.1)
                .unwrap_or("")
                .trim();
            if !body.is_empty() && body != Self::LIVE_TOOL_BODY_PLACEHOLDER {
                return;
            }
        }
        if let Some(execution) = self.tool_executions.iter_mut().rev().find(|e| {
            e.status == ToolStatus::Running
                && (e.tool_name == tool_name
                    || tool_name.starts_with(&format!("{} ", e.tool_name))
                    || tool_name.contains(&e.tool_name))
        }) {
            execution.status = ToolStatus::Failed;
            execution.result = Some(error.to_string());
        }
        self.current_tool = None;

        let body = format!("Error: {}", error.trim());
        let content = format!("[{tool_name}]\n{body}");
        let msg_id = match self.resolve_tool_row(id, tool_name) {
            Some(msg_id) => {
                if let Some(m) = self.messages.iter_mut().find(|m| m.id == msg_id) {
                    m.content = content;
                }
                msg_id
            }
            None => {
                let msg = Message {
                    id: MessageId::new(),
                    role: MessageRole::Tool,
                    content,
                    timestamp: Utc::now(),
                    metadata: MessageMetadata::default(),
                    sequence: 0,
                };
                let msg_id = msg.id.clone();
                self.add_message(msg);
                msg_id
            }
        };
        if !id.is_empty() {
            self.tool_rows_by_call.remove(id);
            self.record_completed_call(id);
        }
        // Always expand errors — same rationale as complete_tool_execution.
        self.expanded_tools.insert(msg_id);
    }

    pub fn toggle_tool_expanded(&mut self, id: &MessageId) -> bool {
        if self.expanded_tools.remove(id) {
            false
        } else {
            self.expanded_tools.insert(id.clone());
            true
        }
    }

    pub fn is_tool_expanded(&self, id: &MessageId) -> bool {
        self.expanded_tools.contains(id)
    }

    pub fn show_tools(&self) -> bool {
        self.show_tools
    }

    pub fn toggle_show_tools(&mut self) -> bool {
        self.show_tools = !self.show_tools;
        self.show_tools
    }
}
