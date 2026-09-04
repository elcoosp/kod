use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::Terminal;
use ratatui::widgets::Paragraph;

use kod_tui::app::KodApp;
use kod_tui::ui::chat::ChatWidget;
use kod_tui::ui::input::InputWidget;
use kod_tui::ui::agent_panel::AgentPanelWidget;

#[test]
fn test_chat_widget_rendering() {
    let app = KodApp::new();
    let widget = ChatWidget::new();

    let area = Rect::new(0, 0, 80, 24);
    let mut buffer = Buffer::empty(area);
    widget.render(&app, &mut buffer);

    assert!(!buffer.content.is_empty());
}

#[test]
fn test_chat_widget_with_messages() {
    let mut app = KodApp::new();
    app.start_response_stream();
    app.add_response_chunk("Hello from KOD");
    app.complete_response();

    let widget = ChatWidget::new();
    let area = Rect::new(0, 0, 80, 24);
    let mut buffer = Buffer::empty(area);
    widget.render(&app, &mut buffer);

    assert!(!buffer.content.is_empty());
}

#[test]
fn test_input_widget_rendering() {
    let app = KodApp::new();
    let widget = InputWidget::new();

    let area = Rect::new(0, 0, 80, 5);
    let mut buffer = Buffer::empty(area);
    widget.render(&app, &mut buffer);

    assert!(!buffer.content.is_empty());
}

#[test]
fn test_agent_panel_widget_rendering() {
    let mut app = KodApp::new();
    app.add_agent("agent1", vec!["capability1".to_string()]);

    let widget = AgentPanelWidget::new();
    let area = Rect::new(0, 0, 80, 24);
    let mut buffer = Buffer::empty(area);
    widget.render(&app, &mut buffer);

    assert!(!buffer.content.is_empty());
}

#[test]
fn test_widget_layout() {
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();

    terminal
        .draw(|f| {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints(
                    [
                        Constraint::Length(3),
                        Constraint::Min(1),
                        Constraint::Length(3),
                    ]
                    .as_ref(),
                )
                .split(f.area());

            f.render_widget(Paragraph::new("Header"), chunks[0]);
            f.render_widget(Paragraph::new("Content"), chunks[1]);
            f.render_widget(Paragraph::new("Input"), chunks[2]);
        })
        .unwrap();

    let buffer = terminal.backend().buffer();
    assert!(!buffer.content.is_empty());
}
