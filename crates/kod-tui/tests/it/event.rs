use kod_tui::event::{Event, EventHandler, EventPriority, KeyCode};
use std::time::Duration;

#[tokio::test]
async fn test_event_handler_creation() {
    let handler = EventHandler::new(Duration::from_millis(100));
    assert!(handler.is_running());
}

#[tokio::test]
async fn test_event_queue() {
    let handler = EventHandler::new(Duration::from_millis(100));

    handler.push_event(Event::Key(KeyCode::Char('a')));
    handler.push_event(Event::Key(KeyCode::Enter));
    handler.push_event(Event::UserInput("test".to_string()));

    assert_eq!(handler.pending_events(), 3);

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
    let handler = EventHandler::new(Duration::from_millis(100));

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

    let start = std::time::Instant::now();
    let event = handler.next_event().await;

    assert!(start.elapsed() >= Duration::from_millis(5));
    assert!(matches!(event, Event::Tick));
}

#[tokio::test]
async fn test_send_event() {
    let handler = EventHandler::new(Duration::from_millis(100));
    handler.send_event(Event::Tick).await.unwrap();
    assert_eq!(handler.pending_events(), 1);
    let event = handler.next_event().await;
    assert!(matches!(event, Event::Tick));
}

#[tokio::test]
async fn test_priority_event_ordering() {
    let handler = EventHandler::new(Duration::from_millis(100));

    handler.push_event(Event::UserInput("normal1".to_string()));
    handler.push_priority_event(
        Event::System(EventPriority::Critical, "urgent".to_string()),
        EventPriority::Critical,
    );
    handler.push_event(Event::UserInput("normal2".to_string()));

    // Critical should come first
    let event = handler.next_event().await;
    assert!(matches!(event, Event::System(EventPriority::Critical, _)));

    let event = handler.next_event().await;
    assert!(matches!(event, Event::UserInput(ref s) if s == "normal1"));

    let event = handler.next_event().await;
    assert!(matches!(event, Event::UserInput(ref s) if s == "normal2"));
}
