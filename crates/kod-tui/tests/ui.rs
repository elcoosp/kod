use kod_tui::app::{
    SLASH_COMMANDS,InputMode, KodApp, Message};
use kod_tui::ui::{AgentPanelWidget, ChatWidget, CompletionsWidget, InputWidget, StatusWidget};
use kod_types::{MessageId, MessageRole};
use ratatui::backend::TestBackend;
use ratatui::{Terminal, buffer::Buffer};

fn buffer_text(buffer: &Buffer) -> String {
    buffer
        .content()
        .iter()
        .map(|c| c.symbol().to_string())
        .collect()
}

#[test]
fn test_chat_widget_rendering() {
    let mut app = KodApp::new();

    app.add_message(Message {
        id: MessageId::new(),
        role: MessageRole::User,
        content: "Hello".to_string(),
        timestamp: chrono::Utc::now(),
        metadata: Default::default(),
        sequence: 0,
    });

    app.add_message(Message {
        id: MessageId::new(),
        role: MessageRole::Assistant,
        content: "Hi there!".to_string(),
        timestamp: chrono::Utc::now(),
        metadata: Default::default(),
        sequence: 0,
    });

    let _terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    let widget = ChatWidget::new();

    let area = ratatui::layout::Rect::new(0, 0, 80, 20);
    let mut buffer = Buffer::empty(area);
    widget.render(&app, area, &mut buffer);

    let text = buffer_text(&buffer);
    assert!(text.contains("Hello"), "user message must be visible");
    assert!(
        text.contains("Hi there!"),
        "assistant message must be visible"
    );
}

#[test]
fn test_agent_panel_rendering() {
    let mut app = KodApp::new();

    app.add_agent("architect", vec!["planning".to_string()]);
    app.add_agent("coder", vec!["coding".to_string()]);

    let widget = AgentPanelWidget::new();

    let area = ratatui::layout::Rect::new(0, 0, 30, 24);
    let mut buffer = Buffer::empty(area);
    widget.render(&app, area, &mut buffer);

    let text = buffer_text(&buffer);
    assert!(text.contains("architect"), "agent must be listed");
}

#[test]
fn test_input_widget_rendering() {
    let mut app = KodApp::new();
    app.set_input("test input".to_string());

    let widget = InputWidget::new();

    let area = ratatui::layout::Rect::new(0, 0, 80, 3);
    let mut buffer = Buffer::empty(area);
    widget.render(&app, area, &mut buffer);

    let text = buffer_text(&buffer);
    assert!(text.contains("test input"), "input text must be visible");
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

#[test]
fn test_thinking_indicator_visible_while_generating() {
    let mut app = KodApp::new();
    app.begin_generation();

    // Thinking indicator lives in the status bar, not the chat (no duplicate).
    let status = StatusWidget::new();
    let chat = ChatWidget::new();
    let area = ratatui::layout::Rect::new(0, 0, 80, 10);
    let mut buffer = Buffer::empty(area);
    status.render(&app, area, &mut buffer);
    let text = buffer_text(&buffer);
    assert!(
        text.contains("connecting"),
        "status spinner must show while generating, got: {text}"
    );
    let mut buffer = Buffer::empty(area);
    chat.render(&app, area, &mut buffer);
    let text = buffer_text(&buffer);
    assert!(
        !text.contains("connecting") && !text.contains("thinking"),
        "chat must not duplicate spinner, got: {text}"
    );

    app.finish_response("done");
    let mut buffer = Buffer::empty(area);
    status.render(&app, area, &mut buffer);
    let text = buffer_text(&buffer);
    assert!(
        !text.contains("thinking") && !text.contains("connecting"),
        "status spinner must clear after response, got: {text}"
    );
    let mut buffer = Buffer::empty(area);
    chat.render(&app, area, &mut buffer);
    let text = buffer_text(&buffer);
    assert!(
        text.contains("done"),
        "response must be visible, got: {text}"
    );
}

#[test]
fn test_response_chunks_accumulate() {
    let mut app = KodApp::new();
    app.begin_generation();
    app.add_response_chunk("hello ");
    app.add_response_chunk("world");
    app.finish_response("");
    assert_eq!(app.messages().len(), 1);
    assert!(app.messages()[0].content.contains("hello world"));
    assert!(!app.is_generating());
}

#[test]
fn test_generation_error_surfaces_and_clears() {
    let mut app = KodApp::new();
    app.begin_generation();
    assert!(app.is_generating());
    app.fail_generation("boom");
    assert!(!app.is_generating());
    assert!(app.messages()[0].content.contains("boom"));
}

#[test]
fn test_slash_completion_filter_and_accept() {
    let mut app = KodApp::new();
    app.set_input_mode(InputMode::Insert);
    app.set_input("/".to_string());
    assert!(app.show_completions());
    assert_eq!(app.completion_candidates().len(), SLASH_COMMANDS.len());

    app.set_input("/mod".to_string());
    let candidates = app.completion_candidates();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].name, "/model");

    app.accept_completion();
    assert_eq!(app.input(), "/model ");

    app.set_input("hello".to_string());
    assert!(!app.show_completions());
}

#[test]
fn test_completions_popup_renders() {
    let mut app = KodApp::new();
    app.set_input_mode(InputMode::Insert);
    app.set_input("/c".to_string());

    let widget = CompletionsWidget::new();
    let area = ratatui::layout::Rect::new(0, 0, 60, 6);
    let mut buffer = Buffer::empty(area);
    widget.render(&app, area, &mut buffer);

    let text = buffer_text(&buffer);
    assert!(text.contains("/clear"), "matching command must be listed");
}

#[test]
fn test_chat_scrollbar_appears_on_overflow() {
    let mut app = KodApp::new();
    for i in 0..30 {
        app.add_message(Message {
            id: MessageId::new(),
            role: MessageRole::User,
            content: format!("message number {i}"),
            timestamp: chrono::Utc::now(),
            metadata: Default::default(),
            sequence: 0,
        });
    }

    let widget = ChatWidget::new();
    let area = ratatui::layout::Rect::new(0, 0, 80, 10);
    let mut buffer = Buffer::empty(area);
    widget.render(&app, area, &mut buffer);

    let text = buffer_text(&buffer);
    // Slim scrollbar on the right edge: track │ + half-block thumb ▌,
    // ▲ at the top since older content sits above. Pinned live, so no ▼.
    assert!(text.contains("▌"), "slim thumb must show: {text}");
    assert!(text.contains("▲"), "more-above arrow must show: {text}");
    assert!(!text.contains("▼"), "pinned live: no below arrow: {text}");
    assert!(!text.contains("█"), "no fat block: {text}");
}

#[test]
fn test_chat_scrollbar_slim_when_scrolled_back() {
    let mut app = KodApp::new();
    for i in 0..30 {
        app.add_message(Message {
            id: MessageId::new(),
            role: MessageRole::User,
            content: format!("message number {i}"),
            timestamp: chrono::Utc::now(),
            metadata: Default::default(),
            sequence: 0,
        });
    }
    app.scroll_up(15);

    let widget = ChatWidget::new();
    let area = ratatui::layout::Rect::new(0, 0, 80, 10);
    let mut buffer = Buffer::empty(area);
    widget.render(&app, area, &mut buffer);

    let text = buffer_text(&buffer);
    // Scrolled back mid-history: thumb plus both extremity arrows.
    assert!(text.contains("▌"), "slim thumb must show: {text}");
    assert!(text.contains("▲"), "more-above arrow must show: {text}");
    assert!(text.contains("▼"), "more-below arrow must show: {text}");
    assert!(!text.contains("█"), "no fat block: {text}");
}

fn push_msg(app: &mut KodApp, role: MessageRole, content: &str) {
    app.add_message(Message {
        id: MessageId::new(),
        role,
        content: content.to_string(),
        timestamp: chrono::Utc::now(),
        metadata: Default::default(),
        sequence: 0,
    });
}

#[test]
fn test_narrow_window_newest_body_stays_visible() {
    // Regression: logical-line paging clipped wrapped rows, so the newest
    // message body vanished in narrow windows with no way to scroll to it.
    let mut app = KodApp::new();
    push_msg(
        &mut app,
        MessageRole::Assistant,
        "aaa bbb ccc ddd eee fff ggg hhh iii jjj kkk lll mmm nnn ooo ppp qqq rrr sss ttt",
    );
    push_msg(&mut app, MessageRole::User, "my newest question here");

    let widget = ChatWidget::new();
    let area = ratatui::layout::Rect::new(0, 0, 30, 8);
    let mut buffer = Buffer::empty(area);
    widget.render(&app, area, &mut buffer);

    let text = buffer_text(&buffer);
    assert!(
        text.contains("my newest question here"),
        "newest user body must be visible at live bottom, got: {text}"
    );
}

#[test]
fn test_tool_message_renders_header_row() {
    let mut app = KodApp::new();
    push_msg(
        &mut app,
        MessageRole::Tool,
        "[list_files path=.] \n3 entries in .: a, b, c",
    );

    let widget = ChatWidget::new();
    let area = ratatui::layout::Rect::new(0, 0, 60, 6);
    let mut buffer = Buffer::empty(area);
    widget.render(&app, area, &mut buffer);

    let text = buffer_text(&buffer);
    assert!(text.contains("list_files"), "tool header missing: {text}");
    assert!(text.contains("3 entries"), "tool body missing: {text}");
}

#[test]
fn test_scroll_to_top_reaches_oldest() {
    let mut app = KodApp::new();
    for i in 0..30 {
        push_msg(&mut app, MessageRole::User, &format!("message number {i}"));
    }
    app.scroll_to_top();

    let widget = ChatWidget::new();
    let area = ratatui::layout::Rect::new(0, 0, 80, 10);
    let mut buffer = Buffer::empty(area);
    widget.render(&app, area, &mut buffer);

    let text = buffer_text(&buffer);
    assert!(
        text.contains("message number 0"),
        "oldest must show: {text}"
    );
    assert!(
        text.contains("▌"),
        "at top, slim thumb marks newer below: {text}"
    );
    assert!(!text.contains("▲"), "at oldest: no above arrow: {text}");
    assert!(text.contains("▼"), "more-below arrow must show: {text}");
}

#[test]
fn test_assistant_message_has_rounded_border() {
    let mut app = KodApp::new();
    push_msg(&mut app, MessageRole::Assistant, "bordered reply");

    let widget = ChatWidget::new();
    let area = ratatui::layout::Rect::new(0, 0, 60, 10);
    let mut buffer = Buffer::empty(area);
    widget.render(&app, area, &mut buffer);

    let text = buffer_text(&buffer);
    assert!(text.contains("bordered reply"), "body missing: {text}");
    assert!(text.contains("╭"), "rounded top-left missing: {text}");
    assert!(text.contains("╮"), "rounded top-right missing: {text}");
    assert!(text.contains("╰"), "rounded bottom-left missing: {text}");
    assert!(text.contains("╯"), "rounded bottom-right missing: {text}");
    assert!(text.contains("│"), "border sides missing: {text}");
}

#[test]
fn test_no_clock_timestamps_in_chat() {
    let mut app = KodApp::new();
    push_msg(&mut app, MessageRole::User, "plain user line");
    push_msg(&mut app, MessageRole::Assistant, "plain ai line");
    let stamp = app.messages()[0].timestamp.format("%H:%M").to_string();

    let widget = ChatWidget::new();
    let area = ratatui::layout::Rect::new(0, 0, 60, 12);
    let mut buffer = Buffer::empty(area);
    widget.render(&app, area, &mut buffer);

    let text = buffer_text(&buffer);
    assert!(
        text.contains("plain user line"),
        "user body missing: {text}"
    );
    assert!(text.contains("plain ai line"), "ai body missing: {text}");
    assert!(
        !text.contains(stamp.as_str()),
        "clock timestamp must not render: {text}"
    );
}

#[test]
fn test_streaming_and_finished_share_border_frame() {
    let mut app = KodApp::new();
    app.begin_generation();
    app.add_response_chunk("half written");

    let widget = ChatWidget::new();
    let area = ratatui::layout::Rect::new(0, 0, 60, 10);
    let mut buffer = Buffer::empty(area);
    widget.render(&app, area, &mut buffer);
    let live = buffer_text(&buffer);
    assert!(live.contains("half written"), "live chunk missing: {live}");
    assert!(
        live.contains("╭"),
        "live text must already be bordered: {live}"
    );

    app.finish_response("");
    let mut buffer = Buffer::empty(area);
    widget.render(&app, area, &mut buffer);
    let done = buffer_text(&buffer);
    assert!(
        done.contains("half written"),
        "finished body missing: {done}"
    );
    assert!(done.contains("╭"), "finished text keeps the border: {done}");
}

#[test]
fn test_messages_render_in_chronological_order() {
    use chrono::TimeZone;
    let mut app = KodApp::new();
    let early = chrono::Utc.with_ymd_and_hms(2026, 9, 4, 12, 0, 1).unwrap();
    let late = chrono::Utc.with_ymd_and_hms(2026, 9, 4, 12, 0, 2).unwrap();
    // Messages render in insertion (sequence) order — never by wall-clock
    // timestamp. Insert chronologically and assert that order is preserved
    // even if timestamps were to collide.
    app.add_message(Message {
        id: MessageId::new(),
        role: MessageRole::User,
        content: "first line".to_string(),
        timestamp: early,
        metadata: Default::default(),
        sequence: 0,
    });
    app.add_message(Message {
        id: MessageId::new(),
        role: MessageRole::Assistant,
        content: "second line".to_string(),
        timestamp: late,
        metadata: Default::default(),
        sequence: 0,
    });

    let widget = ChatWidget::new();
    let area = ratatui::layout::Rect::new(0, 0, 60, 12);
    let mut buffer = Buffer::empty(area);
    widget.render(&app, area, &mut buffer);

    let text = buffer_text(&buffer);
    let first = text.find("first line").expect("first missing: {text}");
    let second = text.find("second line").expect("second missing: {text}");
    assert!(first < second, "insertion order violated: {text}");
}

#[test]
fn test_new_message_does_not_yank_scrolled_view() {
    let mut app = KodApp::new();
    for i in 0..30 {
        push_msg(&mut app, MessageRole::User, &format!("message number {i}"));
    }
    app.scroll_up(15);
    assert!(!app.is_scrolled_to_bottom());
    push_msg(&mut app, MessageRole::Assistant, "late arrival");
    assert!(
        !app.is_scrolled_to_bottom(),
        "reading position must survive new arrivals"
    );
}

#[test]
fn test_thinking_spinner_advances_with_time() {
    use kod_tui::app::SPINNER_FRAMES;
    let mut app = KodApp::new();
    app.begin_generation();
    let first = app.spinner().to_string();
    assert!(
        SPINNER_FRAMES.contains(&first.as_str()),
        "spinner must show a real frame"
    );
    // One 100ms step always crosses a frame boundary.
    std::thread::sleep(std::time::Duration::from_millis(120));
    let second = app.spinner().to_string();
    assert_ne!(first, second, "spinner must advance without any tick");
    app.finish_response("done");
}
