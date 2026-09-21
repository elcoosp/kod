//! Tool-execution rows on `KodApp`.
//!
//! Extracted from `app/mod.rs` (S9). Covers the lifecycle of a single
//! tool call's rendered row — start / update / complete / fail — plus
//! the expand/collapse state that decides whether its body is shown.

use super::*;

impl KodApp {
    pub fn current_tool(&self) -> Option<&String> {
        self.current_tool.as_ref()
    }

    pub fn tool_executions(&self) -> &[ToolExecution] {
        &self.tool_executions
    }

    pub fn start_tool_execution(&mut self, tool_name: &str) {
        self.current_tool = Some(tool_name.to_string());
        self.set_phase(GenPhase::ExecutingTool(tool_name.to_string()));
        self.tool_executions.push(ToolExecution {
            tool_name: tool_name.to_string(),
            status: ToolStatus::Running,
            start_time: Utc::now(),
            result: None,
        });
        // Stream the row live: it lands in position now with a running
        // body, and completion fills that same row in — the call never
        // arrives as a block at task end.
        self.add_message(Message {
            id: MessageId::new(),
            role: MessageRole::Tool,
            content: format!("[{tool_name}]\n{}", Self::LIVE_TOOL_BODY_PLACEHOLDER),
            timestamp: Utc::now(),
            metadata: MessageMetadata::default(),
            sequence: 0,
        });
    }

    /// Index of the most recent tool row still awaiting its result.
    fn live_tool_msg(&self) -> Option<usize> {
        self.messages.iter().rposition(|m| {
            if m.role != MessageRole::Tool {
                return false;
            }
            let body = m.content.split_once('\n').map(|x| x.1).unwrap_or("").trim();
            body == Self::LIVE_TOOL_BODY_PLACEHOLDER
        })
    }

    /// Refresh the live "running …" line with a one-line excerpt of what
    /// the tool is actually doing (`execute_command cargo test …`).
    /// Arrives from the engine after the call's arguments are assembled.
    pub fn update_tool_status(&mut self, display: &str) {
        let display = display.trim();
        if display.is_empty() {
            return;
        }
        self.current_tool = Some(display.to_string());
        self.set_phase(GenPhase::ExecutingTool(display.to_string()));
        // Refresh the live row's header too so the streamed row shows what
        // the call actually does, not just the tool name.
        if let Some(i) = self.live_tool_msg() {
            let body = self.messages[i]
                .content
                .split_once('\n')
                .map(|x| x.1)
                .unwrap_or(Self::LIVE_TOOL_BODY_PLACEHOLDER);
            let body = body.to_string();
            self.messages[i].content = format!("[{display}]\n{body}");
        }
    }

    pub fn complete_tool_execution(&mut self, tool_name: &str, result: &str) {
        self.complete_tool_execution_with_duration(tool_name, result, None);
    }

    /// Fill the live tool row with its result, stamping the header with
    /// wall time when `duration_ms` is present (`header · 1.2s`).
    ///
    /// Idempotent: when no `Running` entry matches `tool_name` and a
    /// finished row for it already exists, this is the task-end fallback
    /// arriving after the live done-marker — keep the live (timed) row
    /// instead of rewriting it without the duration.
    pub fn complete_tool_execution_with_duration(
        &mut self,
        tool_name: &str,
        result: &str,
        duration_ms: Option<u64>,
    ) {
        // `tool_name` here is usually the rendered header
        // (`execute_command command=…`), not the plain name stored at
        // start time — match the running entry by prefix so it actually
        // resolves instead of lingering as Running forever.
        let matched_running = if let Some(execution) =
            self.tool_executions.iter_mut().rev().find(|e| {
                e.status == ToolStatus::Running
                    && (e.tool_name == tool_name
                        || tool_name.starts_with(&format!("{} ", e.tool_name))
                        || tool_name.contains(&e.tool_name))
            }) {
            execution.status = ToolStatus::Completed;
            execution.result = Some(result.to_string());
            true
        } else {
            false
        };

        if !matched_running && self.tool_row_completed_like(tool_name) {
            return;
        }

        self.current_tool = None;
        self.set_phase(GenPhase::Summarizing);

        // Header on its own line: the chat widget renders `[header]` as a
        // `⚙/✗ header` row with the summary body beneath it. Trim blank lines
        // around the body so the row never opens with an empty line.
        // When the row streamed live, fill it in place — no reorder, no
        // duplicate block at task end. Robust: header may have been updated
        // via ToolProgress, so the live placeholder check may miss — fall
        // back to matching by header prefix before appending.
        let body = Self::trim_blank_lines(result);
        let is_error = body.trim_start().starts_with("Error:");
        let header = match duration_ms {
            Some(ms) => format!("{tool_name} · {}", kod_core::engine::format_duration_ms(ms)),
            None => tool_name.to_string(),
        };
        let content = format!("[{header}]\n{body}");
        let msg_id = if let Some(i) = self.live_tool_msg() {
            self.messages[i].content = content;
            self.messages[i].id.clone()
        } else if let Some(i) = self.messages.iter().rposition(|m| {
            m.role == MessageRole::Tool && m.content.starts_with(&format!("[{}]", tool_name))
        }) {
            self.messages[i].content = content;
            self.messages[i].id.clone()
        } else if let Some(i) = self.messages.iter().rposition(|m| {
            // Header was rewritten by ToolProgress — match by tool base name
            let header = m.content.lines().next().unwrap_or("").trim();
            let header = header
                .strip_prefix('[')
                .and_then(|s| s.strip_suffix(']'))
                .unwrap_or(header);
            m.role == MessageRole::Tool && tool_name.contains(header)
                || header.contains(tool_name.split_whitespace().next().unwrap_or(""))
        }) {
            self.messages[i].content = content;
            self.messages[i].id.clone()
        } else {
            let msg = Message {
                id: MessageId::new(),
                role: MessageRole::Tool,
                content,
                timestamp: Utc::now(),
                metadata: MessageMetadata::default(),
                sequence: 0,
            };
            let id = msg.id.clone();
            self.add_message(msg);
            id
        };
        // Errors must be unmistakable: auto-expand so the full message is
        // visible and never hidden behind the 12-line preview.
        if is_error {
            self.expanded_tools.insert(msg_id);
        }
    }

    /// True when a finished (non-placeholder) tool row already exists for
    /// `tool_name`. Used to recognize the task-end fallback arriving after
    /// a live done-marker completed the row — symmetric with the
    /// `Running`-entry match above, plus the reverse direction because the
    /// live row's header carries the `· duration` stamp.
    fn tool_row_completed_like(&self, tool_name: &str) -> bool {
        self.messages.iter().any(|m| {
            if m.role != MessageRole::Tool {
                return false;
            }
            let mut parts = m.content.splitn(2, '\n');
            let first = parts.next().unwrap_or("").trim();
            let header = first
                .strip_prefix('[')
                .and_then(|s| s.strip_suffix(']'))
                .unwrap_or(first);
            let body = parts.next().unwrap_or("").trim();
            if body.is_empty() || body == Self::LIVE_TOOL_BODY_PLACEHOLDER {
                return false;
            }
            header == tool_name
                || header.starts_with(&format!("{tool_name} "))
                || tool_name.starts_with(&format!("{header} "))
                || header.contains(tool_name)
                || tool_name.contains(header)
        })
    }

    pub fn fail_tool_execution(&mut self, tool_name: &str, error: &str) {
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
        let content = format!("[{}]\n{}", tool_name, body);
        let msg_id = if let Some(i) = self.live_tool_msg() {
            self.messages[i].content = content;
            self.messages[i].id.clone()
        } else {
            let msg = Message {
                id: MessageId::new(),
                role: MessageRole::Tool,
                content,
                timestamp: Utc::now(),
                metadata: MessageMetadata::default(),
                sequence: 0,
            };
            let id = msg.id.clone();
            self.add_message(msg);
            id
        };
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
