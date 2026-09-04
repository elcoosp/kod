use kod_tui::app::{AppMode, InputMode, KodApp, Message, ToolStatus};
use kod_types::{MessageMetadata, MessageRole};

#[test]
fn test_app_creation() {
    let app = KodApp::new();
    assert!(!app.should_quit());
    assert_eq!(app.mode(), &AppMode::Normal);
    assert_eq!(app.input_mode(), &InputMode::Normal);
    assert!(app.messages().is_empty());
    assert!(app.agents().is_empty());
}

#[test]
fn test_input_mode_toggle() {
    let mut app = KodApp::new();
    assert_eq!(app.input_mode(), &InputMode::Normal);

    app.set_input_mode(InputMode::Insert);
    assert_eq!(app.input_mode(), &InputMode::Insert);

    app.set_input_mode(InputMode::Normal);
    assert_eq!(app.input_mode(), &InputMode::Normal);
}

#[test]
fn test_add_agent() {
    let mut app = KodApp::new();
    app.add_agent("architect", vec!["planning".to_string(), "research".to_string()]);
    app.add_agent("coder", vec!["coding".to_string()]);

    assert_eq!(app.agents().len(), 2);

    let agents = app.agents_map();
    assert!(agents.contains_key("architect"));
    assert!(agents.contains_key("coder"));

    let agent = agents.get("architect").unwrap();
    assert_eq!(agent.name, "architect");
    assert_eq!(agent.capabilities, vec!["planning", "research"]);
    assert_eq!(agent.status, "idle");
}

#[test]
fn test_add_message() {
    let mut app = KodApp::new();
    app.add_message(Message {
        id: kod_types::MessageId::new(),
        role: MessageRole::User,
        content: "Hello, world!".to_string(),
        timestamp: chrono::Utc::now(),
        metadata: MessageMetadata::default(),
    });

    assert_eq!(app.messages().len(), 1);
    let msg = &app.messages()[0];
    assert_eq!(msg.role, MessageRole::User);
    assert_eq!(msg.content, "Hello, world!");
    assert!(!msg.id.as_uuid().is_nil());
    assert_eq!(msg.role_label(), "You");
}

#[test]
fn test_submit_input() {
    let mut app = KodApp::new();
    app.set_input_mode(InputMode::Insert);
    app.add_char('h');
    app.add_char('i');

    app.submit_input();

    assert_eq!(app.messages().len(), 1);
    let msg = &app.messages()[0];
    assert_eq!(msg.role, MessageRole::User);
    assert_eq!(msg.content, "hi");
    assert_eq!(app.current_input(), "");
}

#[test]
fn test_remove_char() {
    let mut app = KodApp::new();
    app.set_input_mode(InputMode::Insert);
    app.add_char('h');
    app.add_char('i');

    app.remove_char();
    assert_eq!(app.current_input(), "h");

    app.remove_char();
    assert_eq!(app.current_input(), "");

    app.remove_char();
    assert_eq!(app.current_input(), "");
}

#[test]
fn test_mode_switching() {
    let mut app = KodApp::new();
    assert_eq!(app.mode(), &AppMode::Normal);

    app.set_mode(AppMode::AgentPanel);
    assert_eq!(app.mode(), &AppMode::AgentPanel);

    app.set_mode(AppMode::Input);
    assert_eq!(app.mode(), &AppMode::Input);

    app.set_mode(AppMode::Normal);
    assert_eq!(app.mode(), &AppMode::Normal);
}

#[test]
fn test_tool_execution() {
    let mut app = KodApp::new();
    app.start_tool_execution("grep");
    assert_eq!(app.tool_executions().len(), 1);
    assert_eq!(app.tool_executions()[0].tool_name, "grep");
    assert_eq!(app.tool_executions()[0].status, ToolStatus::Running);

    app.complete_tool_execution("grep", "found 3 matches");
    assert_eq!(app.tool_executions()[0].status, ToolStatus::Completed);
    assert_eq!(app.tool_executions()[0].result, Some("found 3 matches".to_string()));
}

#[test]
fn test_agent_status_update() {
    let mut app = KodApp::new();
    app.add_agent("agent1", vec!["task1".to_string()]);

    app.update_agent_status("agent1", "working");
    let agent = app.agents_map().get("agent1").unwrap();
    assert_eq!(agent.status, "working");

    app.update_agent_task("agent1", "implementing feature");
    let agent = app.agents_map().get("agent1").unwrap();
    assert_eq!(agent.current_task, Some("implementing feature".to_string()));
}

#[test]
fn test_serialization() {
    let mut app = KodApp::new();
    app.add_message(Message {
        id: kod_types::MessageId::new(),
        role: MessageRole::User,
        content: "test".to_string(),
        timestamp: chrono::Utc::now(),
        metadata: MessageMetadata::default(),
    });
    app.add_agent("test_agent", vec!["coding".to_string()]);

    let json = serde_json::to_string(&app.messages()).unwrap();
    let parsed: Vec<Message> = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].content, "test");
}

#[test]
fn test_scroll() {
    let mut app = KodApp::new();
    for i in 0..100 {
        app.add_message(Message {
            id: kod_types::MessageId::new(),
            role: MessageRole::User,
            content: format!("Message {i}"),
            timestamp: chrono::Utc::now(),
            metadata: MessageMetadata::default(),
        });
    }
    // Verify we're at the bottom after adding messages
    assert_eq!(app.scroll_offset(), 99); // 100 messages - 1

    // Scroll down 5 from current position
    app.scroll_down(5);
    assert_eq!(app.scroll_offset(), 104); // 99 + 5 = 104 (saturating_add)
}

#[test]
fn test_quit() {
    let mut app = KodApp::new();
    assert!(!app.should_quit());
    app.quit();
    assert!(app.should_quit());
    app.set_should_quit(false);
    assert!(!app.should_quit());
}
