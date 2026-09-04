use kod_tui::app::{AppMode, InputMode, KodApp, Message};
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

    app.set_input_mode(InputMode::Insert);
    assert_eq!(app.input_mode(), &InputMode::Insert);

    app.add_char('h');
    app.add_char('e');
    app.add_char('l');
    app.add_char('l');
    app.add_char('o');

    assert_eq!(app.input(), "hello");

    app.backspace();
    assert_eq!(app.input(), "hell");

    app.clear_input();
    assert_eq!(app.input(), "");
}

#[test]
fn test_input_history() {
    let mut app = KodApp::new();

    app.set_input("first".to_string());
    app.submit_input();

    app.set_input("second".to_string());
    app.submit_input();

    app.set_input("third".to_string());
    app.submit_input();

    assert_eq!(app.input_history().len(), 3);

    app.history_previous();

    app.history_next();
}

#[test]
fn test_mode_switching() {
    let mut app = KodApp::new();

    assert_eq!(app.mode(), &AppMode::Normal);

    app.set_mode(AppMode::AgentPanel);
    assert_eq!(app.mode(), &AppMode::AgentPanel);

    app.set_mode(AppMode::ToolExecution);
    assert_eq!(app.mode(), &AppMode::ToolExecution);

    app.set_mode(AppMode::Help);
    assert_eq!(app.mode(), &AppMode::Help);

    app.set_mode(AppMode::Normal);
    assert_eq!(app.mode(), &AppMode::Normal);
}

#[test]
fn test_agent_status() {
    let mut app = KodApp::new();

    app.add_agent("architect", vec!["planning".to_string(), "research".to_string()]);
    app.add_agent("coder", vec!["coding".to_string()]);

    assert_eq!(app.agents().len(), 2);

    app.update_agent_status("architect", "running");

    let agent = app.get_agent("architect").unwrap();
    assert_eq!(agent.status, "running");
}

#[test]
fn test_tool_execution() {
    let mut app = KodApp::new();

    app.start_tool_execution("read_file");

    assert_eq!(app.current_tool(), Some(&"read_file".to_string()));

    app.complete_tool_execution("read_file", "File contents...");

    assert!(app.messages().iter().any(|m| {
        m.role == MessageRole::Tool && m.content.contains("read_file")
    }));
}

#[test]
fn test_streaming_response() {
    let mut app = KodApp::new();

    app.start_response_stream();

    app.add_response_chunk("Hello");
    app.add_response_chunk(" world");

    assert_eq!(app.current_response(), "Hello world");

    app.complete_response();

    assert!(app.messages().iter().any(|m| {
        m.role == MessageRole::Assistant && m.content == "Hello world"
    }));

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

    for i in 0..50 {
        app.add_message(Message {
            id: MessageId::new(),
            role: MessageRole::User,
            content: format!("Message {}", i),
            timestamp: chrono::Utc::now(),
            metadata: Default::default(),
        });
    }

    assert!(app.is_scrolled_to_bottom());

    app.scroll_up(5);
    assert!(!app.is_scrolled_to_bottom());

    app.scroll_to_bottom();
    assert!(app.is_scrolled_to_bottom());
}

#[test]
fn test_serialization() {
    let mut app = KodApp::new();
    app.add_message(Message {
        id: MessageId::new(),
        role: MessageRole::User,
        content: "test".to_string(),
        timestamp: chrono::Utc::now(),
        metadata: Default::default(),
    });

    let json = serde_json::to_string(&app.messages()).unwrap();
    let parsed: Vec<Message> = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].content, "test");
}
