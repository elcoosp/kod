//! Event handling for the TUI.
//!
//! Provides a unified event system that handles keyboard input,
//! mouse events, ticks, and custom application events.

use crossterm::event::{Event as CrosstermEvent, EventStream};
use std::time::Duration;
use tokio::sync::mpsc;

/// Priority level for events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum EventPriority {
    High,
    Normal,
    Low,
}

/// Unified event type for the TUI.
#[derive(Debug, Clone)]
pub enum Event {
    Key(crossterm::event::KeyEvent),
    Mouse(crossterm::event::MouseEvent),
    Resize(u16, u16),
    Tick,
    UserInput(String),
    System(EventPriority, String),
}

impl Event {
    /// Returns the priority of this event.
    pub fn priority(&self) -> EventPriority {
        match self {
            Event::System(EventPriority::High, _) => EventPriority::High,
            Event::System(EventPriority::Low, _) => EventPriority::Low,
            Event::System(EventPriority::Normal, _) => EventPriority::Normal,
            _ => EventPriority::Normal,
        }
    }
}

impl From<CrosstermEvent> for Event {
    fn from(event: CrosstermEvent) -> Self {
        match event {
            CrosstermEvent::Key(key_event) => Event::Key(key_event),
            CrosstermEvent::Mouse(mouse_event) => Event::Mouse(mouse_event),
            CrosstermEvent::Resize(w, h) => Event::Resize(w, h),
            _ => Event::Tick,
        }
    }
}

use std::collections::VecDeque;

/// Handles input events and tick events with a priority queue.
pub struct EventHandler {
    event_queue: std::sync::Mutex<VecDeque<(EventPriority, Event)>>,
    event_tx: mpsc::Sender<Event>,
    event_rx: std::sync::Mutex<Option<mpsc::Receiver<Event>>>,
    _input_stream: std::sync::Mutex<Option<EventStream>>,
    tick_rate: Duration,
    is_running: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl EventHandler {
    /// Create a new EventHandler with the given tick rate.
    pub fn new(tick_rate: Duration) -> Self {
        let (tx, rx) = mpsc::channel(100);
        Self {
            event_queue: std::sync::Mutex::new(VecDeque::new()),
            event_tx: tx,
            event_rx: std::sync::Mutex::new(Some(rx)),
            _input_stream: std::sync::Mutex::new(None),
            tick_rate,
            is_running: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        }
    }

    /// Check if the handler is running.
    pub fn is_running(&self) -> bool {
        self.is_running.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Stop the handler.
    pub fn stop(&self) {
        self.is_running
            .store(false, std::sync::atomic::Ordering::Relaxed);
    }

    /// Push an event into the queue (normal priority).
    pub fn push_event(&self, event: Event) {
        self.push_priority_event(event, EventPriority::Normal);
    }

    /// Push an event with a specific priority.
    pub fn push_priority_event(&self, event: Event, priority: EventPriority) {
        let mut queue = self.event_queue.lock().unwrap();
        match priority {
            EventPriority::High => queue.push_front((priority, event)),
            _ => queue.push_back((priority, event)),
        }
    }

    /// Send an event through the mpsc channel.
    pub async fn send_event(&self, event: Event) -> crate::app::Result<()> {
        self.event_tx.send(event).await.map_err(|e| {
            std::io::Error::other(e.to_string())
        })?;
        Ok(())
    }

    /// Returns the number of pending events in the queue.
    pub fn pending_events(&self) -> usize {
        self.event_queue.lock().unwrap().len()
    }

    /// Get the next event, either from the queue or by waiting for input/tick.
    pub async fn next_event(&self) -> Event {
        // First, check the priority queue
        {
            let mut queue = self.event_queue.lock().unwrap();
            if let Some((_, event)) = queue.pop_front() {
                return event;
            }
        }

        // Wait for either an input event or a tick
        let sleep = tokio::time::sleep(self.tick_rate);
        tokio::pin!(sleep);

        let mut rx = self.event_rx.lock().unwrap().take().unwrap();
        let result = tokio::select! {
            event = rx.recv() => {
                if let Some(event) = event {
                    event
                } else {
                    Event::Tick
                }
            }
            _ = &mut sleep => {
                Event::Tick
            }
        };

        // Put the receiver back
        *self.event_rx.lock().unwrap() = Some(rx);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_event_handler_creation() {
        let handler = EventHandler::new(Duration::from_millis(100));
        assert!(handler.is_running());
    }

    #[tokio::test]
    async fn test_event_queue() {
        let handler = EventHandler::new(Duration::from_millis(100));
        handler.push_event(Event::UserInput("message1".to_string()));
        handler.push_event(Event::UserInput("message2".to_string()));
        handler.push_event(Event::UserInput("message3".to_string()));
        assert_eq!(handler.pending_events(), 3);

        let _e1 = handler.next_event().await;
        let _e2 = handler.next_event().await;
        let _e3 = handler.next_event().await;
        assert_eq!(handler.pending_events(), 0);
    }

    #[tokio::test]
    async fn test_tick_events() {
        let handler = EventHandler::new(Duration::from_millis(10));
        let start = std::time::Instant::now();
        let event = handler.next_event().await;
        let elapsed = start.elapsed();
        assert!(elapsed >= Duration::from_millis(5));
        assert!(matches!(event, Event::Tick));
    }

    #[tokio::test]
    async fn test_priority_events() {
        let handler = EventHandler::new(Duration::from_millis(100));

        handler.push_event(Event::UserInput("low priority message".to_string()));
        handler.push_priority_event(
            Event::System(EventPriority::High, "high priority".to_string()),
            EventPriority::High,
        );

        {
            let queue = handler.event_queue.lock().unwrap();
            assert_eq!(queue.len(), 2);
            assert_eq!(queue[0].0, EventPriority::High);
        }

        let event1 = handler.next_event().await;
        assert!(matches!(event1, Event::System(_, _)));
        assert_eq!(event1.priority(), EventPriority::High);

        let event2 = handler.next_event().await;
        assert!(matches!(event2, Event::UserInput(_)));
        assert_eq!(event2.priority(), EventPriority::Normal);
    }

    #[tokio::test]
    async fn test_event_priority() {
        let handler = EventHandler::new(Duration::from_millis(100));

        handler.push_event(Event::UserInput("first".to_string()));
        handler.push_priority_event(
            Event::System(EventPriority::High, "urgent".to_string()),
            EventPriority::High,
        );

        let event1 = handler.next_event().await;
        assert_eq!(event1.priority(), EventPriority::High);

        let event2 = handler.next_event().await;
        assert_eq!(event2.priority(), EventPriority::Normal);
    }
}
