//! Application state for the TUI.
//!
//! Manages messages, input, agent status, tool execution state,
//! and scrolling.

use kod_types::{MessageMetadata, MessageRole, MessageId};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use chrono::{DateTime, Utc};

/// Result type for TUI operations
pub type Result<T> = std::result::Result<T, std::io::Error>;

/// Message displayed in the chat
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: MessageId,
    pub role: MessageRole,
    pub content: String,
    pub timestamp: DateTime<Utc>,
    pub metadata: MessageMetadata,
}

impl Message {
    /// Returns a display label for the message role
    pub fn role_label(&self) -> &str {
        match self.role {
            MessageRole::User => "You",
            MessageRole::Assistant => "KOD",
            MessageRole::Tool => "Tool",
            MessageRole::System => "System",
            MessageRole::Agent(_) => "Agent",
        }
    }
}

/// Agent status information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentInfo {
    pub name: String,
    pub capabilities: Vec<String>,
    pub status: String,
    pub current_task: Option<String>,
}

/// Tool execution state
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolExecution {
    pub tool_name: String,
    pub status: ToolStatus,
    pub start_time: DateTime<Utc>,
    pub result: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolStatus {
    Running,
    Completed,
    Failed,
}

/// Application modes
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppMode {
    Normal,
    AgentPanel,
    ToolExecution,
    Help,
    Input,
}

/// Input modes
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    Normal,
    Insert,
}

/// Main application state
pub struct KodApp {
    mode: AppMode,
    input_mode: InputMode,
    input: String,
    cursor_position: usize,
    input_history: Vec<String>,
    history_index: Option<usize>,

    messages: Vec<Message>,
    scroll_position: usize,

    agents: HashMap<String, AgentInfo>,
    tool_executions: Vec<ToolExecution>,
    current_tool: Option<String>,

    current_response: String,
    is_streaming: bool,

    should_quit: bool,
}

impl KodApp {
    pub fn new() -> Self {
        Self {
            mode: AppMode::Normal,
            input_mode: InputMode::Normal,
            input: String::new(),
            cursor_position: 0,
            input_history: Vec::new(),
            history_index: None,

            messages: Vec::new(),
            scroll_position: 0,

            agents: HashMap::new(),
            tool_executions: Vec::new(),
            current_tool: None,

            current_response: String::new(),
            is_streaming: false,

            should_quit: false,
        }
    }

    // Mode management
    pub fn mode(&self) -> &AppMode {
        &self.mode
    }

    pub fn set_mode(&mut self, mode: AppMode) {
        self.mode = mode;
    }

    pub fn input_mode(&self) -> &InputMode {
        &self.input_mode
    }

    pub fn set_input_mode(&mut self, mode: InputMode) {
        self.input_mode = mode;
    }

    // Input handling
    pub fn input(&self) -> &str {
        &self.input
    }

    pub fn current_input(&self) -> &str {
        &self.input
    }

    pub fn set_input(&mut self, input: String) {
        self.input = input;
        self.cursor_position = self.input.len();
    }

    pub fn add_char(&mut self, c: char) {
        self.input.insert(self.cursor_position, c);
        self.cursor_position += 1;
    }

    pub fn backspace(&mut self) {
        if self.cursor_position > 0 {
            self.cursor_position -= 1;
            self.input.remove(self.cursor_position);
        }
    }

    pub fn remove_char(&mut self) {
        if self.cursor_position > 0 {
            self.cursor_position -= 1;
            self.input.remove(self.cursor_position);
        }
    }

    pub fn clear_input(&mut self) {
        self.input.clear();
        self.cursor_position = 0;
    }

    pub fn submit_input(&mut self) {
        if !self.input.is_empty() {
            self.input_history.push(self.input.clone());
            self.add_message(Message {
                id: MessageId::new(),
                role: MessageRole::User,
                content: self.input.clone(),
                timestamp: Utc::now(),
                metadata: MessageMetadata::default(),
            });
            self.clear_input();
            self.history_index = None;
        }
    }

    pub fn input_history(&self) -> &[String] {
        &self.input_history
    }

    pub fn history_previous(&mut self) {
        if self.input_history.is_empty() {
            return;
        }

        match self.history_index {
            None => {
                self.history_index = Some(self.input_history.len() - 1);
                self.set_input(self.input_history[self.input_history.len() - 1].clone());
            }
            Some(0) => {}
            Some(index) => {
                self.history_index = Some(index - 1);
                self.set_input(self.input_history[index - 1].clone());
            }
        }
    }

    pub fn history_next(&mut self) {
        if self.input_history.is_empty() {
            return;
        }

        match self.history_index {
            None => {}
            Some(index) if index >= self.input_history.len() - 1 => {
                self.history_index = None;
                self.clear_input();
            }
            Some(index) => {
                self.history_index = Some(index + 1);
                self.set_input(self.input_history[index + 1].clone());
            }
        }
    }

    // Message management
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub fn add_message(&mut self, message: Message) {
        self.messages.push(message);
        self.scroll_to_bottom();
    }

    pub fn add_agent_message(&mut self, content: String) {
        self.add_message(Message {
            id: MessageId::new(),
            role: MessageRole::Assistant,
            content,
            timestamp: Utc::now(),
            metadata: MessageMetadata::default(),
        });
    }

    pub fn scroll_up(&mut self, lines: usize) {
        self.scroll_position = self.scroll_position.saturating_sub(lines);
    }

    pub fn scroll_down(&mut self, lines: usize) {
        self.scroll_position = self.scroll_position.saturating_add(lines);
    }

    pub fn scroll_to_bottom(&mut self) {
        self.scroll_position = self.messages.len().saturating_sub(1);
    }

    pub fn scroll_offset(&self) -> usize {
        self.scroll_position
    }

    pub fn is_scrolled_to_bottom(&self) -> bool {
        self.scroll_position >= self.messages.len().saturating_sub(1)
    }

    // Agent management
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

    // Tool execution
    pub fn current_tool(&self) -> Option<&String> {
        self.current_tool.as_ref()
    }

    pub fn tool_executions(&self) -> &[ToolExecution] {
        &self.tool_executions
    }

    pub fn start_tool_execution(&mut self, tool_name: &str) {
        self.current_tool = Some(tool_name.to_string());
        self.tool_executions.push(ToolExecution {
            tool_name: tool_name.to_string(),
            status: ToolStatus::Running,
            start_time: Utc::now(),
            result: None,
        });
    }

    pub fn complete_tool_execution(&mut self, tool_name: &str, result: &str) {
        if let Some(execution) = self
            .tool_executions
            .iter_mut()
            .rev()
            .find(|e| e.tool_name == tool_name && e.status == ToolStatus::Running)
        {
            execution.status = ToolStatus::Completed;
            execution.result = Some(result.to_string());
        }

        self.current_tool = None;

        self.add_message(Message {
            id: MessageId::new(),
            role: MessageRole::Tool,
            content: format!("[{}] {}", tool_name, result),
            timestamp: Utc::now(),
            metadata: MessageMetadata::default(),
        });
    }

    pub fn fail_tool_execution(&mut self, tool_name: &str, error: &str) {
        if let Some(execution) = self
            .tool_executions
            .iter_mut()
            .rev()
            .find(|e| e.tool_name == tool_name && e.status == ToolStatus::Running)
        {
            execution.status = ToolStatus::Failed;
            execution.result = Some(error.to_string());
        }

        self.current_tool = None;

        self.add_message(Message {
            id: MessageId::new(),
            role: MessageRole::Tool,
            content: format!("[{}] Error: {}", tool_name, error),
            timestamp: Utc::now(),
            metadata: MessageMetadata::default(),
        });
    }

    // Streaming response
    pub fn current_response(&self) -> &str {
        &self.current_response
    }

    pub fn is_streaming(&self) -> bool {
        self.is_streaming
    }

    pub fn start_response_stream(&mut self) {
        self.is_streaming = true;
        self.current_response.clear();
    }

    pub fn add_response_chunk(&mut self, chunk: &str) {
        if self.is_streaming {
            self.current_response.push_str(chunk);
        }
    }

    pub fn complete_response(&mut self) {
        if self.is_streaming {
            let response = self.current_response.clone();
            self.add_message(Message {
                id: MessageId::new(),
                role: MessageRole::Assistant,
                content: response,
                timestamp: Utc::now(),
                metadata: MessageMetadata::default(),
            });
            self.is_streaming = false;
            self.current_response.clear();
        }
    }

    // Quit handling
    pub fn should_quit(&self) -> bool {
        self.should_quit
    }

    pub fn quit(&mut self) {
        self.should_quit = true;
    }

    pub fn set_should_quit(&mut self, quit: bool) {
        self.should_quit = quit;
    }
}

impl Default for KodApp {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_app_lifecycle() {
        let mut app = KodApp::new();

        app.set_input_mode(InputMode::Insert);
        app.add_char('t');
        app.add_char('e');
        app.add_char('s');
        app.add_char('t');

        assert_eq!(app.input(), "test");

        app.submit_input();
        assert_eq!(app.messages().len(), 1);
        assert_eq!(app.input(), "");
    }

    #[test]
    fn test_agent_management() {
        let mut app = KodApp::new();
        app.add_agent("agent1", vec!["coding".to_string()]);
        assert_eq!(app.agents().len(), 1);
        assert!(app.get_agent("agent1").is_some());
        app.update_agent_status("agent1", "working");
        assert_eq!(app.get_agent("agent1").unwrap().status, "working");
    }

    #[test]
    fn test_message_display() {
        let mut app = KodApp::new();
        app.add_message(Message {
            id: MessageId::new(),
            role: MessageRole::User,
            content: "hello".to_string(),
            timestamp: Utc::now(),
            metadata: MessageMetadata::default(),
        });

        let msg = &app.messages()[0];
        assert_eq!(msg.role_label(), "You");
    }
}
