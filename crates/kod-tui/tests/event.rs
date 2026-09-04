use kod_tui::event::{Event, EventPriority, EventHandler};
use std::time::Duration;

#[test]
fn test_event_priority_enum() {
    assert_eq!(EventPriority::High as u8, 0);  // High should be highest priority (for Ord)
    assert_eq!(EventPriority::Normal as u8, 1);
    assert_eq!(EventPriority::Low as u8, 2);
}

#[test]
fn test_event_priority_ordering() {
    assert!(EventPriority::High < EventPriority::Normal);
    assert!(EventPriority::Normal < EventPriority::Low);
}

#[tokio::test]
async fn test_event_priority_normal() {
    let _handler = EventHandler::new(Duration::from_millis(100));
    let event = Event::UserInput("test".to_string());
    assert_eq!(event.priority(), EventPriority::Normal);
}

#[tokio::test]
async fn test_event_priority_high() {
    let _handler = EventHandler::new(Duration::from_millis(100));
    let event = Event::System(EventPriority::High, "critical".to_string());
    assert_eq!(event.priority(), EventPriority::High);
}

#[tokio::test]
async fn test_event_priority_low() {
    let _handler = EventHandler::new(Duration::from_millis(100));
    let event = Event::System(EventPriority::Low, "background".to_string());
    assert_eq!(event.priority(), EventPriority::Low);
}

#[tokio::test]
async fn test_push_and_retrieve_priority() {
    let handler = EventHandler::new(Duration::from_millis(100));
    handler.push_event(Event::UserInput("low priority message".to_string()));
    handler.push_priority_event(
        Event::System(EventPriority::High, "high priority".to_string()),
        EventPriority::High,
    );

    // High priority should be first
    let event1 = handler.next_event().await;
    assert!(matches!(event1, Event::System(_, _)));
    assert_eq!(event1.priority(), EventPriority::High);

    let event2 = handler.next_event().await;
    assert!(matches!(event2, Event::UserInput(_)));
    assert_eq!(event2.priority(), EventPriority::Normal);
}
