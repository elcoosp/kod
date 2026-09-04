use kod_tui::app::{KodApp, Message};
use kod_tui::ui::{AgentPanelWidget, ChatWidget, InputWidget};
use kod_types::{MessageId, MessageRole};
use ratatui::backend::TestBackend;
use ratatui::{Terminal, buffer::Buffer};

#[test]
fn test_chat_widget_rendering() {
    let mut app = KodApp::new();

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

    let _terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    let widget = ChatWidget::new();

    let area = ratatui::layout::Rect::new(0, 0, 80, 20);
    let mut buffer = Buffer::empty(area);
    widget.render(&app, &mut buffer);

    let content = buffer.content();
    assert!(!content.is_empty());
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
    assert!(!content.is_empty());
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
    assert!(!content.is_empty());
}

#[test]
fn test_widget_layout() {
    let _terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    let area = ratatui::layout::Rect::new(0, 0, 80, 24);

    let layout = ratatui::layout::Layout::default()
        .direction(ratatui::layout::Direction::Vertical)
        .constraints([
            ratatui::layout::Constraint::Length(3),
            ratatui::layout::Constraint::Min(1),
            ratatui::layout::Constraint::Length(3),
        ])
        .split(area);

    assert_eq!(layout[0].height, 3);
    assert_eq!(layout[1].height, 18);
    assert_eq!(layout[2].height, 3);
}
