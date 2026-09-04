# Chunk 8: TUI Implementation

## Task 40: TUI Foundation and Event Handling

**Files:**
- Modify: `crates/kod-tui/Cargo.toml`
- Create: `crates/kod-tui/src/lib.rs`
- Create: `crates/kod-tui/src/event.rs`
- Create: `crates/kod-tui/src/app.rs`
- Test: `crates/kod-tui/tests/event.rs`

- [ ] **Step 1: Update kod-tui Cargo.toml**

```toml
[package]
name = "kod-tui"
version.workspace = true
edition.workspace = true
license.workspace = true
authors.workspace = true

[dependencies]
ratatui = { workspace = true }
crossterm = { workspace = true }
tokio = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
tracing = { workspace = true }
uuid = { version = "1.11", features = ["v4"] }
chrono = { version = "0.4", features = ["serde"] }
kod-types = { path = "../kod-types" }
kod-error = { path = "../kod-error" }
kod-core = { path = "../kod-core" }

[dev-dependencies]
rstest = { workspace = true }
tempfile = "3.8"
```

- [ ] **Step 2: Write failing test for event handling**

Create `crates/kod-tui/tests/event.rs`:

```rust
use kod_tui::event::{Event, EventHandler, KeyCode};
use std::time::Duration;

#[tokio::test]
async fn test_event_handler_creation() {
    let handler = EventHandler::new(Duration::from_millis(100));
    assert!(handler.is_running());
}

#[tokio::test]
async fn test_event_queue() {
    let mut handler = EventHandler::new(Duration::from_millis(100));
    
    // Push events to queue
    handler.push_event(Event::Key(KeyCode::Char('a')));
    handler.push_event(Event::Key(KeyCode::Enter));
    handler.push_event(Event::UserInput("test".to_string()));
    
    assert_eq!(handler.pending_events(), 3);
    
    // Pop events
    let event1 = handler.next_event().await;
    assert!(matches!(event1, Event::Key(KeyCode::Char('a'))));
    
    let event2 = handler.next_event().await;
    assert!(matches!(event2, Event::Key(KeyCode::Enter)));
    
    let event3 = handler.next_event().await;
    assert!(matches!(event3, Event::UserInput(ref s) if s == "test"));
    
    assert_eq!(handler.pending_events(), 0);
}

#[tokio::test]
async fn test_event_priority() {
    let mut handler = EventHandler::new(Duration::from_millis(100));
    
    // Push different priority events
    handler.push_event(Event::UserInput("normal".to_string()));
    handler.push_event(Event::System(EventPriority::High, "critical".to_string()));
    handler.push_event(Event::UserInput("another".to_string()));
    
    // High priority event should come first
    let event = handler.next_event().await;
    if let Event::System(priority, message) = event {
        assert_eq!(priority, EventPriority::High);
        assert_eq!(message, "critical");
    } else {
        panic!("Expected high priority system event");
    }
}

#[test]
fn test_event_serialization() {
    let event = Event::UserInput("test message".to_string());
    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains("test message"));
    
    let deserialized: Event = serde_json::from_str(&json).unwrap();
    match deserialized {
        Event::UserInput(s) => assert_eq!(s, "test message"),
        _ => panic!("Expected UserInput event"),
    }
}

#[tokio::test]
async fn test_tick_events() {
    let handler = EventHandler::new(Duration::from_millis(10));
    
    // Wait for tick
    let start = std::time::Instant::now();
    let event = handler.next_event().await;
    
    assert!(start.elapsed() >= Duration::from_millis(5));
    assert!(matches!(event, Event::Tick));
}
```

- [ ] **Step 3: Run tests to verify they fail**

```bash
cargo test -p kod-tui --test event
```

Expected: FAIL - event module not implemented

- [ ] **Step 4: Implement event handling**

Create `crates/kod-tui/src/lib.rs`:

```rust
//! Terminal UI for KOD - provides interactive interface.
//!
//! This crate implements the terminal user interface using Ratatui,
//! including chat display, agent panel, input handling, and tool execution display.

pub mod event;
pub mod app;
pub mod ui;
pub mod components;

pub use event::{Event, EventHandler, EventPriority, KeyCode};
pub use app::KodApp;
```

Create `crates/kod-tui/src/event.rs`:

```rust
//! Event handling for the TUI.
//!
//! Provides a unified event system that handles keyboard input,
//! mouse events, ticks, and custom application events.

use crossterm::event::{Event as CrosstermEvent, EventStream, KeyCode as CrosstermKeyCode, KeyEvent};
use futures::StreamExt;
use kod_error::{KodError, Result};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Mutex};
use tokio::time::interval;

/// Key codes we care about
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum KeyCode {
    Char(char),
    Enter,
    Escape,
    Backspace,
    Delete,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Tab,
    BackTab,
    F(u8),
}

impl From<CrosstermKeyCode> for KeyCode {
    fn from(code: CrosstermKeyCode) -> Self {
        match code {
            CrosstermKeyCode::Char(c) => KeyCode::Char(c),
            CrosstermKeyCode::Enter => KeyCode::Enter,
            CrosstermKeyCode::Esc => KeyCode::Escape,
            CrosstermKeyCode::Backspace => KeyCode::Backspace,
            CrosstermKeyCode::Delete => KeyCode::Delete,
            CrosstermKeyCode::Up => KeyCode::Up,
            CrosstermKeyCode::Down => KeyCode::Down,
            CrosstermKeyCode::Left => KeyCode::Left,
            CrosstermKeyCode::Right => KeyCode::Right,
            CrosstermKeyCode::Home => KeyCode::Home,
            CrosstermKeyCode::End => KeyCode::End,
            CrosstermKeyCode::PageUp => KeyCode::PageUp,
            CrosstermKeyCode::PageDown => KeyCode::PageDown,
            CrosstermKeyCode::Tab => KeyCode::Tab,
            CrosstermKeyCode::BackTab => KeyCode::BackTab,
            CrosstermKeyCode::F(n) => KeyCode::F(n),
            _ => KeyCode::Char(' '), // Default for unhandled keys
        }
    }
}

/// Priority for events
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum EventPriority {
    Low,
    Normal,
    High,
    Critical,
}

impl Default for EventPriority {
    fn default() -> Self {
        EventPriority::Normal
    }
}

/// Events that can occur in the TUI
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {
    /// Keyboard input
    Key(KeyCode),
    
    /// User submitted input
    UserInput(String),
    
    /// Timer tick
    Tick,
    
    /// System message
    System(EventPriority, String),
    
    /// Tool execution started
    ToolStarted(String),
    
    /// Tool execution completed
    ToolCompleted(String, String), // (tool_name, result)
    
    /// Agent message received
    AgentMessage(String, String), // (agent_name, message)
    
    /// LLM response chunk (for streaming)
    ResponseChunk(String),
    
    /// LLM response complete
    ResponseComplete(String),
    
    /// Error occurred
    Error(String),
    
    /// Quit the application
    Quit,
    
    /// Resize event
    Resize(u16, u16),
}

/// Event handler that manages the event loop
pub struct EventHandler {
    // Queue for pending events
    event_queue: Mutex<VecDeque<(EventPriority, Event)>>,
    
    // Channel for receiving events
    event_rx: Mutex<mpsc::Receiver<Event>>,
    event_tx: mpsc::Sender<Event>,
    
    // Tick interval
    tick_rate: Duration,
    
    // Running state
    is_running: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl EventHandler {
    /// Create a new event handler
    pub fn new(tick_rate: Duration) -> Self {
        let (tx, rx) = mpsc::channel(100);
        
        Self {
            event_queue: Mutex::new(VecDeque::new()),
            event_rx: Mutex::new(rx),
            event_tx: tx,
            tick_rate,
            is_running: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        }
    }

    /// Check if handler is running
    pub fn is_running(&self) -> bool {
        self.is_running.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Get number of pending events
    pub fn pending_events(&self) -> usize {
        self.event_queue.blocking_lock().len()
    }

    /// Push an event to the queue
    pub fn push_event(&self, event: Event) {
        let mut queue = self.event_queue.blocking_lock();
        queue.push_back((EventPriority::Normal, event));
    }

    /// Push a high-priority event
    pub fn push_priority_event(&self, event: Event, priority: EventPriority) {
        let mut queue = self.event_queue.blocking_lock();
        
        // Find insert position based on priority
        let insert_pos = queue.iter()
            .position(|(p, _)| *p < priority)
            .unwrap_or(queue.len());
        
        queue.insert(insert_pos, (priority, event));
    }

    /// Get the next event (waits if no events)
    pub async fn next_event(&self) -> Event {
        // First check the priority queue
        {
            let mut queue = self.event_queue.lock().await;
            if let Some((_, event)) = queue.pop_front() {
                return event;
            }
        }
        
        // If no events in queue, wait for new events or tick
        let mut rx = self.event_rx.lock().await;
        
        let tick_deadline = tokio::time::Instant::from_std(
            Instant::now() + self.tick_rate
        );
        
        tokio::select! {
            event = rx.recv() => {
                if let Some(event) = event {
                    return event;
                }
            }
            _ = tokio::time::sleep_until(tick_deadline) => {
                return Event::Tick;
            }
        }
        
        // If channel closed, return tick as fallback
        Event::Tick
    }

    /// Send an event (from external sources)
    pub async fn send_event(&self, event: Event) -> Result<()> {
        self.event_tx.send(event)
            .await
            .map_err(|e| KodError::Internal(format!("Failed to send event: {}", e)))
    }

    /// Start the input handling loop
    pub async fn start_input_loop(&self) {
        let tx = self.event_tx.clone();
        let is_running = self.is_running.clone();
        
        tokio::spawn(async move {
            let mut reader = EventStream::new();
            
            while is_running.load(std::sync::atomic::Ordering::SeqCst) {
                if let Some(Ok(event)) = reader.next().await {
                    match event {
                        CrosstermEvent::Key(key) => {
                            if key.kind == crossterm::event::KeyEventKind::Press {
                                let key_code: KeyCode = key.code.into();
                                let _ = tx.send(Event::Key(key_code)).await;
                            }
                        }
                        CrosstermEvent::Resize(w, h) => {
                            let _ = tx.send(Event::Resize(w, h)).await;
                        }
                        _ => {} // Ignore other events
                    }
                }
            }
        });
    }

    /// Stop the event handler
    pub fn stop(&self) {
        self.is_running.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_event_queue() {
        let handler = EventHandler::new(Duration::from_millis(100));
        
        handler.push_event(Event::UserInput("test".to_string()));
        assert_eq!(handler.pending_events(), 1);
        
        let event = handler.next_event().await;
        assert!(matches!(event, Event::UserInput(s) if s == "test"));
    }

    #[test]
    fn test_priority_events() {
        let handler = EventHandler::new(Duration::from_millis(100));
        
        handler.push_event(Event::UserInput("normal".to_string()));
        handler.push_priority_event(
            Event::System(EventPriority::High, "critical".to_string()),
            EventPriority::High,
        );
        
        let queue = handler.event_queue.blocking_lock();
        assert_eq!(queue.len(), 2);
        
        // First should be high priority
        assert_eq!(queue[0].0, EventPriority::High);
    }
}
```

- [ ] **Step 5: Run tests to verify they pass**

```bash
cargo test -p kod-tui --test event
cargo test -p kod-tui --lib event
```

Expected: All tests pass

- [ ] **Step 6: Commit**

```bash
git add crates/kod-tui/
git commit -m "feat(tui): add event handling system with priority queue"
```

---

## Task 41: App State Management

**Files:**
- Create: `crates/kod-tui/src/app.rs`
- Test: `crates/kod-tui/tests/app.rs`

- [ ] **Step 1: Write failing test for app state**

Create `crates/kod-tui/tests/app.rs`:

```rust
use kod_tui::app::{KodApp, AppMode, InputMode, Message};
use kod_types::{MessageRole, MessageId};

#[test]
fn test_app_creation() {
    let app = KodApp::new();
    
    assert_eq!(app.mode(), &AppMode::Normal);
    assert_eq!(app.input_mode(), &InputMode::Normal);
    assert!(app.messages().is_empty());
    assert!(app.input().is_empty());
}

#[test]
fn test_add_message() {
    let mut app = KodApp::new();
    
    let message = Message {
        id: MessageId::new(),
        role: MessageRole::User,
        content: "Hello".to_string(),
        timestamp: chrono::Utc::now(),
        metadata: Default::default(),
    };
    
    app.add_message(message.clone());
    
    assert_eq!(app.messages().len(), 1);
    assert_eq!(app.messages()[0].content, "Hello");
}

#[test]
fn test_input_handling() {
    let mut app = KodApp::new();
    
    // Enter insert mode
    app.set_input_mode(InputMode::Insert);
    assert_eq!(app.input_mode(), &InputMode::Insert);
    
    // Add characters
    app.add_char('h');
    app.add_char('e');
    app.add_char('l');
    app.add_char('l');
    app.add_char('o');
    
    assert_eq!(app.input(), "hello");
    
    // Backspace
    app.backspace();
    assert_eq!(app.input(), "hell");
    
    // Clear input
    app.clear_input();
    assert_eq!(app.input(), "");
}

#[test]
fn test_input_history() {
    let mut app = KodApp::new();
    
    // Submit multiple inputs
    app.set_input("first".to_string());
    app.submit_input();
    
    app.set_input("second".to_string());
    app.submit_input();
    
    app.set_input("third".to_string());
    app.submit_input();
    
    // History should have 3 items
    assert_eq!(app.input_history().len(), 3);
    
    // Navigate history
    app.history_previous();
    // Current input should be "second" (previous from empty)
    
    app.history_next();
    // Current input should be "third" or empty
}

#[test]
fn test_mode_switching() {
    let mut app = KodApp::new();
    
    // Start in normal mode
    assert_eq!(app.mode(), &AppMode::Normal);
    
    // Switch to different modes
    app.set_mode(AppMode::AgentPanel);
    assert_eq!(app.mode(), &AppMode::AgentPanel);
    
    app.set_mode(AppMode::ToolExecution);
    assert_eq!(app.mode(), &AppMode::ToolExecution);
    
    app.set_mode(AppMode::Help);
    assert_eq!(app.mode(), &AppMode::Help);
    
    // Return to normal
    app.set_mode(AppMode::Normal);
    assert_eq!(app.mode(), &AppMode::Normal);
}

#[test]
fn test_agent_status() {
    let mut app = KodApp::new();
    
    // Add agents
    app.add_agent("architect", vec!["planning", "research".to_string()]);
    app.add_agent("coder", vec!["coding".to_string()]);
    
    assert_eq!(app.agents().len(), 2);
    
    // Update agent status
    app.update_agent_status("architect", "running");
    
    let agent = app.get_agent("architect").unwrap();
    assert_eq!(agent.status, "running");
}

#[test]
fn test_tool_execution() {
    let mut app = KodApp::new();
    
    // Start tool execution
    app.start_tool_execution("read_file");
    
    // Should be in tool execution mode
    assert_eq!(app.current_tool(), Some(&"read_file".to_string()));
    
    // Complete tool execution
    app.complete_tool_execution("read_file", "File contents...");
    
    // Should have tool result in messages
    assert!(app.messages().iter().any(|m| {
        m.role == MessageRole::Tool && m.content.contains("read_file")
    }));
}

#[test]
fn test_streaming_response() {
    let mut app = KodApp::new();
    
    // Start streaming
    app.start_response_stream();
    
    // Add chunks
    app.add_response_chunk("Hello");
    app.add_response_chunk(" world");
    
    // Should have partial response
    assert_eq!(app.current_response(), "Hello world");
    
    // Complete response
    app.complete_response();
    
    // Should have complete message
    assert!(app.messages().iter().any(|m| {
        m.role == MessageRole::Assistant && m.content == "Hello world"
    }));
    
    // Current response should be cleared
    assert_eq!(app.current_response(), "");
}

#[test]
fn test_should_quit() {
    let mut app = KodApp::new();
    
    assert!(!app.should_quit());
    
    app.quit();
    assert!(app.should_quit());
}

#[test]
fn test_scroll_position() {
    let mut app = KodApp::new();
    
    // Add many messages
    for i in 0..50 {
        app.add_message(Message {
            id: MessageId::new(),
            role: MessageRole::User,
            content: format!("Message {}", i),
            timestamp: chrono::Utc::now(),
            metadata: Default::default(),
        });
    }
    
    // Scroll position starts at bottom
    assert!(app.is_scrolled_to_bottom());
    
    // Scroll up
    app.scroll_up(5);
    assert!(!app.is_scrolled_to_bottom());
    
    // Scroll to bottom
    app.scroll_to_bottom();
    assert!(app.is_scrolled_to_bottom());
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test -p kod-tui --test app
```

Expected: FAIL - app module not implemented

- [ ] **Step 3: Implement app state**

Create `crates/kod-tui/src/app.rs`:

```rust
//! Application state for the TUI.

use kod_types::{MessageId, MessageMetadata, MessageRole};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use chrono::{DateTime, Utc};

/// Message displayed in the chat
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: MessageId,
    pub role: MessageRole,
    pub content: String,
    pub timestamp: DateTime<Utc>,
    pub metadata: MessageMetadata,
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

    pub fn clear_input(&mut self) {
        self.input.clear();
        self.cursor_position = 0;
    }

    pub fn submit_input(&mut self) {
        if !self.input.is_empty() {
            // Add to history
            self.input_history.push(self.input.clone());
            
            // Add as user message
            self.add_message(Message {
                id: MessageId::new(),
                role: MessageRole::User,
                content: self.input.clone(),
                timestamp: Utc::now(),
                metadata: MessageMetadata::default(),
            });
            
            // Clear input
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
            Some(0) => {
                // At the beginning, do nothing
            }
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
            None => {
                // Not in history, do nothing
            }
            Some(index) if index >= self.input_history.len() - 1 => {
                // At the end, clear input
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
        // Auto-scroll to bottom
        self.scroll_to_bottom();
    }

    pub fn scroll_up(&mut self, lines: usize) {
        self.scroll_position = self.scroll_position.saturating_sub(lines);
    }

    pub fn scroll_down(&mut self, lines: usize) {
        self.scroll_position = self.scroll_position.saturating_add(lines);
    }

    pub fn scroll_to_bottom(&mut self) {
        self.scroll_position = usize::MAX;
    }

    pub fn is_scrolled_to_bottom(&self) -> bool {
        self.scroll_position >= self.messages.len().saturating_sub(1)
    }

    // Agent management
    pub fn agents(&self) -> Vec<&AgentInfo> {
        self.agents.values().collect()
    }

    pub fn add_agent(&mut self, name: &str, capabilities: Vec<String>) {
        self.agents.insert(name.to_string(), AgentInfo {
            name: name.to_string(),
            capabilities,
            status: "idle".to_string(),
            current_task: None,
        });
    }

    pub fn get_agent(&self, name: &str) -> Option<&AgentInfo> {
        self.agents.get(name)
    }

    pub fn update_agent_status(&mut self, name: &str, status: &str) {
        if let Some(agent) = self.agents.get_mut(name) {
            agent.status = status.to_string();
        }
    }

    // Tool execution
    pub fn current_tool(&self) -> Option<&String> {
        self.current_tool.as_ref()
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
        if let Some(execution) = self.tool_executions.iter_mut().rev().find(|e| {
            e.tool_name == tool_name && e.status == ToolStatus::Running
        }) {
            execution.status = ToolStatus::Completed;
            execution.result = Some(result.to_string());
        }
        
        self.current_tool = None;
        
        // Add tool result as message
        self.add_message(Message {
            id: MessageId::new(),
            role: MessageRole::Tool,
            content: format!("[{}] {}", tool_name, result),
            timestamp: Utc::now(),
            metadata: MessageMetadata::default(),
        });
    }

    pub fn fail_tool_execution(&mut self, tool_name: &str, error: &str) {
        if let Some(execution) = self.tool_executions.iter_mut().rev().find(|e| {
            e.tool_name == tool_name && e.status == ToolStatus::Running
        }) {
            execution.status = ToolStatus::Failed;
            execution.result = Some(error.to_string());
        }
        
        self.current_tool = None;
        
        // Add error as message
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
}
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
cargo test -p kod-tui --test app
cargo test -p kod-tui --lib app
```

Expected: All tests pass

- [ ] **Step 5: Commit**

```bash
git add crates/kod-tui/
git commit -m "feat(tui): add application state management with messages, agents, and tools"
```

---

## Task 42: UI Components and Rendering

**Files:**
- Create: `crates/kod-tui/src/ui/mod.rs`
- Create: `crates/kod-tui/src/ui/chat.rs`
- Create: `crates/kod-tui/src/ui/agent_panel.rs`
- Create: `crates/kod-tui/src/ui/input.rs`
- Create: `crates/kod-tui/src/components/mod.rs`
- Test: `crates/kod-tui/tests/ui.rs`

- [ ] **Step 1: Write failing test for UI components**

Create `crates/kod-tui/tests/ui.rs`:

```rust
use kod_tui::ui::{ChatWidget, AgentPanelWidget, InputWidget};
use kod_tui::app::{KodApp, Message};
use kod_types::{MessageId, MessageRole};
use ratatui::backend::TestBackend;
use ratatui::{Terminal, buffer::Buffer};

#[test]
fn test_chat_widget_rendering() {
    let mut app = KodApp::new();
    
    // Add test messages
    app.add_message(Message {
        id: MessageId::new(),
        role: MessageRole::User,
        content: "Hello".to_string(),
        timestamp: chrono::Utc::now(),
        metadata: Default::default(),
    });
    
    app.add_message(Message {
        id: MessageId::new(),
        role: MessageRole::Assistant,
        content: "Hi there!".to_string(),
        timestamp: chrono::Utc::now(),
        metadata: Default::default(),
    });
    
    let terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    let widget = ChatWidget::new();
    
    // Should render without panicking
    let area = ratatui::layout::Rect::new(0, 0, 80, 20);
    let mut buffer = Buffer::empty(area);
    widget.render(&app, &mut buffer);
    
    // Check some content is rendered
    let content = buffer.content();
    assert!(content.len() > 0);
}

#[test]
fn test_agent_panel_rendering() {
    let mut app = KodApp::new();
    
    app.add_agent("architect", vec!["planning".to_string()]);
    app.add_agent("coder", vec!["coding".to_string()]);
    
    let widget = AgentPanelWidget::new();
    
    let area = ratatui::layout::Rect::new(0, 0, 30, 24);
    let mut buffer = Buffer::empty(area);
    widget.render(&app, &mut buffer);
    
    let content = buffer.content();
    assert!(content.len() > 0);
}

#[test]
fn test_input_widget_rendering() {
    let mut app = KodApp::new();
    app.set_input("test input".to_string());
    
    let widget = InputWidget::new();
    
    let area = ratatui::layout::Rect::new(0, 0, 80, 3);
    let mut buffer = Buffer::empty(area);
    widget.render(&app, &mut buffer);
    
    let content = buffer.content();
    assert!(content.len() > 0);
}

#[test]
fn test_widget_layout() {
    let terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    let area = ratatui::layout::Rect::new(0, 0, 80, 24);
    
    // Layout should divide space correctly
    let layout = ratatui::layout::Layout::default()
        .direction(ratatui::layout::Direction::Vertical)
        .constraints([
            ratatui::layout::Constraint::Length(3),  // Header
            ratatui::layout::Constraint::Min(1),     // Main area
            ratatui::layout::Constraint::Length(3),  // Input
        ])
        .split(area);
    
    assert_eq!(layout[0].height, 3);
    assert_eq!(layout[1].height, 18); // 24 - 3 - 3
    assert_eq!(layout[2].height, 3);
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test -p kod-tui --test ui
```

Expected: FAIL - UI modules not implemented

- [ ] **Step 3: Implement UI components**

Create `crates/kod-tui/src/ui/mod.rs`:

```rust
//! UI rendering components.

pub mod chat;
pub mod agent_panel;
pub mod input;
pub mod header;

pub use chat::ChatWidget;
pub use agent_panel::AgentPanelWidget;
pub use input::InputWidget;
pub use header::HeaderWidget;
```

Create `crates/kod-tui/src/ui/chat.rs`:

```rust
//! Chat message display widget.

use crate::app::{KodApp, Message};
use kod_types::MessageRole;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Widget, Wrap};

/// Widget for displaying chat messages
pub struct ChatWidget;

impl ChatWidget {
    pub fn new() -> Self {
        Self
    }

    pub fn render(&self, app: &KodApp, area: &mut Buffer) {
        let rect = Rect {
            x: area.area.x,
            y: area.area.y,
            width: area.area.width,
            height: area.area.height,
        };
        
        let mut lines: Vec<Line> = Vec::new();
        
        // Get visible messages (simple implementation, no scrolling yet)
        let messages: Vec<&Message> = app.messages().iter().collect();
        
        for message in messages {
            let (prefix, style) = match message.role {
                MessageRole::User => (
                    "[You] ",
                    Style::default().fg(Color::Green),
                ),
                MessageRole::Assistant => (
                    "[AI] ",
                    Style::default().fg(Color::Cyan),
                ),
                MessageRole::System => (
                    "[System] ",
                    Style::default().fg(Color::Yellow),
                ),
                MessageRole::Tool => (
                    "[Tool] ",
                    Style::default().fg(Color::Magenta),
                ),
                MessageRole::Agent(_) => (
                    "[Agent] ",
                    Style::default().fg(Color::Blue),
                ),
            };
            
            // Add timestamp
            let timestamp = message.timestamp.format("%H:%M:%S");
            
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{} ", timestamp),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(prefix.to_string(), style.clone()),
            ]));
            
            // Add content (wrap long lines)
            let content_lines: Vec<&str> = message.content.lines().collect();
            for (i, line) in content_lines.iter().enumerate() {
                if i == 0 {
                    lines.push(Line::from(vec![
                        Span::raw("  "),
                        Span::styled(*line, style),
                    ]));
                } else {
                    lines.push(Line::from(format!("  {}", line)));
                }
            }
            
            // Add spacing between messages
            lines.push(Line::from(""));
        }
        
        // Add streaming response if present
        if app.is_streaming() {
            lines.push(Line::from(vec![
                Span::styled("[AI] ", Style::default().fg(Color::Cyan)),
                Span::styled("(streaming...)", Style::default().fg(Color::DarkGray)),
            ]));
            
            for line in app.current_response().lines() {
                lines.push(Line::from(format!("  {}", line)));
            }
        }
        
        let text = Text::from(lines);
        let paragraph = Paragraph::new(text)
            .wrap(Wrap { trim: false });
        
        paragraph.render(rect, area);
    }
}

impl Default for ChatWidget {
    fn default() -> Self {
        Self::new()
    }
}
```

Create `crates/kod-tui/src/ui/agent_panel.rs`:

```rust
//! Agent status panel widget.

use crate::app::KodApp;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget, Wrap};

/// Widget for displaying agent status
pub struct AgentPanelWidget;

impl AgentPanelWidget {
    pub fn new() -> Self {
        Self
    }

    pub fn render(&self, app: &KodApp, area: &mut Buffer) {
        let rect = Rect {
            x: area.area.x,
            y: area.area.y,
            width: area.area.width,
            height: area.area.height,
        };
        
        let mut lines: Vec<Line> = Vec::new();
        
        // Header
        lines.push(Line::from(vec![
            Span::styled(
                "Agents",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
        
        lines.push(Line::from("─".repeat(area.area.width as usize)));
        
        // Agent list
        let agents: Vec<_> = app.agents().iter().collect();
        
        if agents.is_empty() {
            lines.push(Line::from("No active agents"));
        } else {
            for agent in agents {
                // Status indicator
                let status_color = match agent.status.as_str() {
                    "running" => Color::Green,
                    "idle" => Color::Gray,
                    "error" => Color::Red,
                    _ => Color::White,
                };
                
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("● ", ),
                        Style::default().fg(status_color),
                    ),
                    Span::styled(
                        agent.name.clone(),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                ]));
                
                // Capabilities
                if !agent.capabilities.is_empty() {
                    let caps = agent.capabilities.join(", ");
                    lines.push(Line::from(format!(
                        "  caps: {}",
                        caps
                    )));
                }
                
                // Current task
                if let Some(task) = &agent.current_task {
                    lines.push(Line::from(format!(
                        "  task: {}",
                        task
                    )));
                }
                
                // Status
                lines.push(Line::from(format!(
                    "  status: {}",
                    agent.status
                )));
                
                lines.push(Line::from(""));
            }
        }
        
        // Tool executions
        lines.push(Line::from(vec![
            Span::styled(
                "Tool Executions",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
        
        if let Some(current_tool) = app.current_tool() {
            lines.push(Line::from(format!(
                "▶ {} (running...)",
                current_tool
            )));
        }
        
        let text = ratatui::text::Text::from(lines);
        let paragraph = Paragraph::new(text)
            .wrap(Wrap { trim: false });
        
        paragraph.render(rect, area);
    }
}

impl Default for AgentPanelWidget {
    fn default() -> Self {
        Self::new()
    }
}
```

Create `crates/kod-tui/src/ui/input.rs`:

```rust
//! Input widget.

use crate::app::{InputMode, KodApp};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

/// Widget for user input
pub struct InputWidget;

impl InputWidget {
    pub fn new() -> Self {
        Self
    }

    pub fn render(&self, app: &KodApp, area: &mut Buffer) {
        let rect = Rect {
            x: area.area.x,
            y: area.area.y,
            width: area.area.width,
            height: area.area.height,
        };
        
        let (title, style) = match app.input_mode() {
            InputMode::Normal => (
                " Normal ",
                Style::default().fg(Color::Blue),
            ),
            InputMode::Insert => (
                " Input ",
                Style::default().fg(Color::Green),
            ),
        };
        
        let mut spans = vec![
            Span::styled(
                "❯ ",
                Style::default().fg(Color::Cyan),
            ),
        ];
        
        if app.input().is_empty() {
            if *app.input_mode() == InputMode::Insert {
                spans.push(Span::styled(
                    "Type your message...",
                    Style::default().fg(Color::DarkGray),
                ));
            } else {
                spans.push(Span::styled(
                    "Press 'i' to enter input mode",
                    Style::default().fg(Color::DarkGray),
                ));
            }
        } else {
            spans.push(Span::raw(app.input().to_string()));
            
            // Add cursor
            if *app.input_mode() == InputMode::Insert {
                spans.push(Span::styled(
                    "▌",
                    Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
                ));
            }
        }
        
        let line = Line::from(spans);
        let text = ratatui::text::Text::from(vec![line]);
        
        let paragraph = Paragraph::new(text);
        
        paragraph.render(rect, area);
    }
}

impl Default for InputWidget {
    fn default() -> Self {
        Self::new()
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
cargo test -p kod-tui --test ui
```

Expected: All tests pass

- [ ] **Step 5: Commit**

```bash
git add crates/kod-tui/
git commit -m "feat(tui): add UI widgets for chat, agent panel, and input"
```

---

## Task 43: Main TUI Loop

**Files:**
- Modify: `crates/kod-tui/src/lib.rs`
- Create: `crates/kod-tui/src/main_loop.rs`
- Test: `crates/kod-tui/tests/main_loop.rs`

- [ ] **Step 1: Write failing test for main loop**

Create `crates/kod-tui/tests/main_loop.rs`:

```rust
use kod_tui::main_loop::TuiLoop;
use kod_tui::event::{Event, KeyCode};

#[tokio::test]
async fn test_tui_loop_creation() {
    let tui = TuiLoop::new();
    assert!(tui.is_initialized());
}

#[tokio::test]
async fn test_tui_event_processing() {
    let mut tui = TuiLoop::new();
    
    // Send some events
    tui.handle_event(Event::Key(KeyCode::Char('i'))).await.unwrap();
    tui.handle_event(Event::Key(KeyCode::Char('h'))).await.unwrap();
    tui.handle_event(Event::Key(KeyCode::Char('i'))).await.unwrap();
    
    // Input mode should be insert
    assert!(tui.app().input_mode() == &kod_tui::app::InputMode::Insert);
    
    // Input should have characters
    assert_eq!(tui.app().input(), "hi");
}

#[tokio::test]
async fn test_tui_input_submission() {
    let mut tui = TuiLoop::new();
    
    // Enter insert mode and type
    tui.handle_event(Event::Key(KeyCode::Char('i'))).await.unwrap();
    tui.handle_event(Event::Key(KeyCode::Char('t'))).await.unwrap();
    tui.handle_event(Event::Key(KeyCode::Char('e'))).await.unwrap();
    tui.handle_event(Event::Key(KeyCode::Char('s'))).await.unwrap();
    tui.handle_event(Event::Key(KeyCode::Char('t'))).await.unwrap();
    
    // Submit
    tui.handle_event(Event::Key(KeyCode::Enter)).await.unwrap();
    
    // Should have user message
    assert_eq!(tui.app().messages().len(), 1);
    assert!(tui.app().messages()[0].content.contains("test"));
}

#[tokio::test]
async fn test_tui_quit() {
    let mut tui = TuiLoop::new();
    
    // Press escape to quit
    tui.handle_event(Event::Key(KeyCode::Escape)).await.unwrap();
    
    // Should want to quit
    assert!(tui.app().should_quit());
}

#[tokio::test]
async fn test_tui_mode_switching() {
    let mut tui = TuiLoop::new();
    
    // Default is normal mode
    assert_eq!(tui.app().mode(), &kod_tui::app::AppMode::Normal);
    
    // Switch to agent panel
    tui.handle_event(Event::Key(KeyCode::Tab)).await.unwrap();
    assert_eq!(tui.app().mode(), &kod_tui::app::AppMode::AgentPanel);
    
    // Switch back
    tui.handle_event(Event::Key(KeyCode::Tab)).await.unwrap();
    assert_eq!(tui.app().mode(), &kod_tui::app::AppMode::Normal);
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test -p kod-tui --test main_loop
```

Expected: FAIL - main_loop module not implemented

- [ ] **Step 3: Implement main TUI loop**

Create `crates/kod-tui/src/main_loop.rs`:

```rust
//! Main TUI loop - coordinates rendering and event handling.

use crate::{
    app::{AppMode, InputMode, KodApp},
    event::{Event, EventHandler, KeyCode},
    ui::{AgentPanelWidget, ChatWidget, InputWidget},
};
use kod_error::{KodError, Result};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    Terminal,
};
use std::io::Stdout;
use std::time::Duration;

/// Main TUI application loop
pub struct TuiLoop {
    app: KodApp,
    event_handler: EventHandler,
    terminal: Option<Terminal<CrosstermBackend<Stdout>>>,
}

impl TuiLoop {
    pub fn new() -> Self {
        Self {
            app: KodApp::new(),
            event_handler: EventHandler::new(Duration::from_millis(100)),
            terminal: None,
        }
    }

    pub fn is_initialized(&self) -> bool {
        true // Always initialized in this implementation
    }

    pub fn app(&self) -> &KodApp {
        &self.app
    }

    pub fn app_mut(&mut self) -> &mut KodApp {
        &mut self.app
    }

    /// Initialize terminal
    pub async fn init_terminal(&mut self) -> Result<()> {
        crossterm::terminal::enable_raw_mode()
            .map_err(|e| KodError::Internal(format!("Failed to enable raw mode: {}", e)))?;
        
        crossterm::execute!(
            std::io::stdout(),
            crossterm::terminal::EnterAlternateScreen,
            crossterm::event::EnableMouseCapture
        ).map_err(|e| KodError::Internal(format!("Failed to enter alternate screen: {}", e)))?;
        
        let backend = CrosstermBackend::new(std::io::stdout());
        let terminal = Terminal::new(backend)
            .map_err(|e| KodError::Internal(format!("Failed to create terminal: {}", e)))?;
        
        self.terminal = Some(terminal);
        
        // Start event handler
        self.event_handler.start_input_loop().await;
        
        Ok(())
    }

    /// Restore terminal
    pub async fn restore_terminal(&mut self) -> Result<()> {
        self.event_handler.stop();
        
        if let Some(terminal) = &mut self.terminal {
            terminal.show_cursor()
                .map_err(|e| KodError::Internal(format!("Failed to show cursor: {}", e)))?;
        }
        
        crossterm::execute!(
            std::io::stdout(),
            crossterm::terminal::LeaveAlternateScreen,
            crossterm::event::DisableMouseCapture
        ).map_err(|e| KodError::Internal(format!("Failed to leave alternate screen: {}", e)))?;
        
        crossterm::terminal::disable_raw_mode()
            .map_err(|e| KodError::Internal(format!("Failed to disable raw mode: {}", e)))?;
        
        self.terminal = None;
        
        Ok(())
    }

    /// Run the main loop
    pub async fn run(&mut self) -> Result<()> {
        self.init_terminal().await?;
        
        let result = self.main_loop().await;
        
        // Always restore terminal
        let _ = self.restore_terminal().await;
        
        result
    }

    /// Internal main loop
    async fn main_loop(&mut self) -> Result<()> {
        while !self.app.should_quit() {
            // Render
            self.render().await?;
            
            // Handle events
            let event = self.event_handler.next_event().await;
            self.handle_event(event).await?;
        }
        
        Ok(())
    }

    /// Render the UI
    async fn render(&mut self) -> Result<()> {
        if let Some(terminal) = &mut self.terminal {
            terminal.draw(|f| {
                let size = f.size();
                
                // Main layout
                let chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([
                        Constraint::Length(1),  // Status bar
                        Constraint::Min(1),     // Main area
                        Constraint::Length(3),  // Input
                    ])
                    .split(size);
                
                // Main area (split horizontally in agent panel mode)
                let main_area = if self.app.mode() == AppMode::AgentPanel {
                    let main_chunks = Layout::default()
                        .direction(Direction::Horizontal)
                        .constraints([
                            Constraint::Percentage(70),  // Chat
                            Constraint::Percentage(30),  // Agent panel
                        ])
                        .split(chunks[1]);
                    
                    // Render chat
                    let chat_widget = ChatWidget::new();
                    let mut chat_buffer = f.current_buffer_mut();
                    chat_widget.render(&self.app, &mut *chat_buffer);
                    
                    // Render agent panel
                    let agent_widget = AgentPanelWidget::new();
                    let mut agent_buffer = f.current_buffer_mut();
                    agent_widget.render(&self.app, &mut *agent_buffer);
                    
                    None // Already rendered
                } else {
                    // Just render chat in full area
                    Some(chunks[1])
                };
                
                if let Some(chat_area) = main_area {
                    let chat_widget = ChatWidget::new();
                    let mut chat_buffer = f.current_buffer_mut();
                    chat_widget.render(&self.app, &mut *chat_buffer);
                }
                
                // Render input
                let input_widget = InputWidget::new();
                let mut input_buffer = f.current_buffer_mut();
                input_widget.render(&self.app, &mut *input_buffer);
                
                // Render status bar
                let status_text = format!(
                    " KOD | Mode: {:?} | Input: {:?} | Messages: {} ",
                    self.app.mode(),
                    self.app.input_mode(),
                    self.app.messages().len()
                );
                
                let status = ratatui::widgets::Paragraph::new(status_text)
                    .style(ratatui::style::Style::default().fg(ratatui::style::Color::White));
                
                f.render_widget(status, chunks[0]);
            }).map_err(|e| KodError::Internal(format!("Failed to draw: {}", e)))?;
        }
        
        Ok(())
    }

    /// Handle a single event
    pub async fn handle_event(&mut self, event: Event) -> Result<()> {
        match event {
            Event::Key(key_code) => self.handle_key(key_code).await?,
            Event::UserInput(input) => {
                self.app.set_input(input);
                self.app.submit_input();
            }
            Event::Quit => {
                self.app.quit();
            }
            Event::Tick => {
                // Periodic updates
            }
            Event::Resize(w, h) => {
                // Handle resize
                tracing::debug!("Terminal resized to {}x{}", w, h);
            }
            Event::ResponseChunk(chunk) => {
                self.app.add_response_chunk(&chunk);
            }
            Event::ResponseComplete(_) => {
                self.app.complete_response();
            }
            Event::ToolStarted(tool_name) => {
                self.app.start_tool_execution(&tool_name);
            }
            Event::ToolCompleted(tool_name, result) => {
                self.app.complete_tool_execution(&tool_name, &result);
            }
            Event::AgentMessage(agent_name, message) => {
                // Add agent message to chat
                self.app.add_message(crate::app::Message {
                    id: kod_types::MessageId::new(),
                    role: kod_types::MessageRole::Agent(kod_types::AgentId::new()),
                    content: format!("[{}] {}", agent_name, message),
                    timestamp: chrono::Utc::now(),
                    metadata: Default::default(),
                });
            }
            Event::Error(error) => {
                // Add error message to chat
                self.app.add_message(crate::app::Message {
                    id: kod_types::MessageId::new(),
                    role: kod_types::MessageRole::System,
                    content: format!("Error: {}", error),
                    timestamp: chrono::Utc::now(),
                    metadata: Default::default(),
                });
            }
            _ => {}
        }
        
        Ok(())
    }

    /// Handle key events
    async fn handle_key(&mut self, key: KeyCode) -> Result<()> {
        match self.app.input_mode() {
            InputMode::Normal => self.handle_normal_mode_key(key).await,
            InputMode::Insert => self.handle_insert_mode_key(key).await,
        }
    }

    /// Handle keys in normal mode
    async fn handle_normal_mode_key(&mut self, key: KeyCode) -> Result<()> {
        match key {
            KeyCode::Char('i') => {
                self.app.set_input_mode(InputMode::Insert);
            }
            KeyCode::Char('q') | KeyCode::Escape => {
                self.app.quit();
            }
            KeyCode::Tab => {
                // Toggle agent panel
                match self.app.mode() {
                    AppMode::Normal => self.app.set_mode(AppMode::AgentPanel),
                    AppMode::AgentPanel => self.app.set_mode(AppMode::Normal),
                    _ => self.app.set_mode(AppMode::Normal),
                }
            }
            KeyCode::Char('h') | KeyCode::Char('?') => {
                self.app.set_mode(AppMode::Help);
            }
            KeyCode::Up => {
                self.app.scroll_up(1);
            }
            KeyCode::Down => {
                self.app.scroll_down(1);
            }
            KeyCode::PageUp => {
                self.app.scroll_up(10);
            }
            KeyCode::PageDown => {
                self.app.scroll_down(10);
            }
            KeyCode::Home => {
                self.app.scroll_to_bottom();
            }
            _ => {}
        }
        
        Ok(())
    }

    /// Handle keys in insert mode
    async fn handle_insert_mode_key(&mut self, key: KeyCode) -> Result<()> {
        match key {
            KeyCode::Enter => {
                self.app.submit_input();
                // Stay in insert mode for continuous input
            }
            KeyCode::Escape => {
                self.app.set_input_mode(InputMode::Normal);
            }
            KeyCode::Backspace => {
                self.app.backspace();
            }
            KeyCode::Up => {
                self.app.history_previous();
            }
            KeyCode::Down => {
                self.app.history_next();
            }
            KeyCode::Char(c) => {
                self.app.add_char(c);
            }
            _ => {}
        }
        
        Ok(())
    }
}

impl Default for TuiLoop {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_tui_lifecycle() {
        let mut tui = TuiLoop::new();
        
        // Test event handling
        tui.handle_event(Event::Key(KeyCode::Char('i'))).await.unwrap();
        assert_eq!(tui.app().input_mode(), &InputMode::Insert);
        
        tui.handle_event(Event::Key(KeyCode::Escape)).await.unwrap();
        assert_eq!(tui.app().input_mode(), &InputMode::Normal);
    }
}
```

- [ ] **Step 4: Update lib.rs exports**

Update `crates/kod-tui/src/lib.rs`:

```rust
//! Terminal UI for KOD - provides interactive interface.
//!
//! This crate implements the terminal user interface using Ratatui,
//! including chat display, agent panel, input handling, and tool execution display.

pub mod event;
pub mod app;
pub mod ui;
pub mod components;
pub mod main_loop;

pub use event::{Event, EventHandler, EventPriority, KeyCode};
pub use app::{KodApp, Message, AppMode, InputMode};
pub use main_loop::TuiLoop;
```

- [ ] **Step 5: Run tests**

```bash
cargo test -p kod-tui
```

Expected: All tests pass

- [ ] **Step 6: Verify workspace builds**

```bash
cargo build --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

Expected: Build succeeds with no warnings

- [ ] **Step 7: Commit**

```bash
git add crates/kod-tui/
git commit -m "feat(tui): add main TUI loop with event handling and rendering"
```

---

## Chunk 8 Review Checklist

- [ ] Event handling system with priority queue
- [ ] Application state management (modes, input, messages)
- [ ] Chat widget renders messages with different roles
- [ ] Agent panel displays agent status and capabilities
- [ ] Input widget with insert/normal modes
- [ ] Main loop coordinates rendering and events
- [ ] Input history navigation
- [ ] Streaming response display
- [ ] Tool execution display
- [ ] Scrolling support
- [ ] Terminal setup/teardown
- [ ] All tests pass
- [ ] Clippy passes with no warnings

**Verification commands:**

```bash
cargo test -p kod-tui
cargo clippy -p kod-tui -- -D warnings
cargo build --workspace
```

---

## Chunk 8 Summary

**Implemented:**
1. **Event Handling** (`event.rs`)
   - Priority-based event queue
   - Keyboard input handling via crossterm
   - Tick events for periodic updates
   - Custom events for LLM responses, tool executions, etc.

2. **Application State** (`app.rs`)
   - Message management with different roles
   - Input handling with history
   - Agent status tracking
   - Tool execution state
   - Streaming response handling
   - Scroll position management

3. **UI Widgets** (`ui/`)
   - ChatWidget: Displays messages with timestamps and role colors
   - AgentPanelWidget: Shows agent status, capabilities, and current tasks
   - InputWidget: Input line with cursor and mode indicator

4. **Main Loop** (`main_loop.rs`)
   - Terminal initialization and cleanup
   - Event processing and dispatch
   - Rendering coordination
   - Mode-based key handling (normal vs insert)
   - Layout management

**Next Chunk Preview:**

Chunk 9 will cover the **CLI Implementation**:
- Command-line interface with clap
- Subcommands (chat, query, swarm, skills, memory)
- Configuration management
- Integration with core engine
- Binary entry point

Would you like me to continue with **Chunk 9: CLI Implementation**?
