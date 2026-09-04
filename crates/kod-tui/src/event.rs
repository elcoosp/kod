//! Event handling for the TUI.
//!
//! Provides a unified event system that handles keyboard input,
//! mouse events, ticks, and custom application events.

use crossterm::event::{
    Event as CrosstermEvent, EventStream, KeyCode as CrosstermKeyCode, KeyEventKind,
};
use futures::StreamExt as _;
use kod_error::Result;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::Mutex as StdMutex;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Mutex as TokioMutex};

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
            _ => KeyCode::Char(' '),
        }
    }
}

/// Priority for events
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
pub enum EventPriority {
    Low,
    #[default]
    Normal,
    High,
    Critical,
}

/// Events that can occur in the TUI
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {
    Key(KeyCode),
    UserInput(String),
    Tick,
    System(EventPriority, String),
    ToolStarted(String),
    ToolCompleted(String, String),
    AgentMessage(String, String),
    ResponseChunk(String),
    ResponseComplete(String),
    Error(String),
    Quit,
    Resize(u16, u16),
}

/// Event handler that manages the event loop
pub struct EventHandler {
    event_queue: StdMutex<VecDeque<(EventPriority, Event)>>,
    event_rx: TokioMutex<mpsc::Receiver<Event>>,
    event_tx: mpsc::Sender<Event>,
    tick_rate: Duration,
    #[allow(dead_code)]
    last_tick: StdMutex<Instant>,
    is_running: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl EventHandler {
    /// Create a new event handler
    pub fn new(tick_rate: Duration) -> Self {
        let (tx, rx) = mpsc::channel(100);
        Self {
            event_queue: StdMutex::new(VecDeque::new()),
            event_rx: TokioMutex::new(rx),
            event_tx: tx,
            tick_rate,
            last_tick: StdMutex::new(Instant::now()),
            is_running: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        }
    }

    /// Check if handler is running
    pub fn is_running(&self) -> bool {
        self.is_running.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Get number of pending events
    pub fn pending_events(&self) -> usize {
        self.event_queue.lock().unwrap().len()
    }

    /// Push an event to the queue
    pub fn push_event(&self, event: Event) {
        let priority = match &event {
            Event::System(p, _) => *p,
            Event::Error(_) => EventPriority::High,
            Event::Quit => EventPriority::Critical,
            _ => EventPriority::Normal,
        };
        self.push_priority_event(event, priority);
    }

    /// Push a high-priority event
    pub fn push_priority_event(&self, event: Event, priority: EventPriority) {
        let mut queue = self.event_queue.lock().unwrap();

        let insert_pos = queue
            .iter()
            .position(|(p, _)| *p < priority)
            .unwrap_or(queue.len());

        queue.insert(insert_pos, (priority, event));
    }

    /// Get the next event (waits if no events)
    pub async fn next_event(&self) -> Event {
        // First check the priority queue
        {
            let mut queue = self.event_queue.lock().unwrap();
            if let Some((_, event)) = queue.pop_front() {
                return event;
            }
        }

        // If no events in queue, wait for new events or tick
        let mut rx = self.event_rx.lock().await;
        let deadline = tokio::time::Instant::now() + self.tick_rate;

        tokio::select! {
            event = rx.recv() => {
                if let Some(event) = event {
                    return event;
                }
            }
            _ = tokio::time::sleep_until(deadline) => {
                return Event::Tick;
            }
        }

        Event::Tick
    }

    /// Send an event (from external sources)
    pub async fn send_event(&self, event: Event) -> Result<()> {
        self.push_event(event);
        Ok(())
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
                            if key.kind == KeyEventKind::Press {
                                let key_code: KeyCode = key.code.into();
                                let _ = tx.send(Event::Key(key_code)).await;
                            }
                        }
                        CrosstermEvent::Resize(w, h) => {
                            let _ = tx.send(Event::Resize(w, h)).await;
                        }
                        _ => {}
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

        let queue = handler.event_queue.lock().unwrap();
        assert_eq!(queue.len(), 2);

        assert_eq!(queue[0].0, EventPriority::High);
    }

    #[test]
    fn test_keycode_from_crossterm() {
        let key: KeyCode = CrosstermKeyCode::Char('a').into();
        assert_eq!(key, KeyCode::Char('a'));

        let key: KeyCode = CrosstermKeyCode::Enter.into();
        assert_eq!(key, KeyCode::Enter);

        let key: KeyCode = CrosstermKeyCode::Esc.into();
        assert_eq!(key, KeyCode::Escape);
    }
}
