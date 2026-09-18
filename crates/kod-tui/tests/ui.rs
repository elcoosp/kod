use kod_tui::app::{InputMode, KodApp, Message, SLASH_COMMANDS};
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

    app.begin_swarm();
    app.swarm_agent_started(
        kod_types::AgentId::new(),
        "architect",
        "plan the schema",
        Some("local-ollama/qwen2.5-coder:7b".to_string()),
    );
    app.swarm_agent_started(
        kod_types::AgentId::new(),
        "coder",
        "write the handler",
        None,
    );

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

/// While the search bar is being edited, printable characters go
/// into the query — not into the input box, and not into the normal
/// mode keybinding dispatch. This is the type-ahead contract the
/// `/search` command relies on.
#[tokio::test]
async fn test_typing_into_search_bar_edits_query() {
    use kod_tui::{Event, KeyCode, TuiLoop};

    let mut tui = TuiLoop::new();
    // Open the search bar (equivalent to running /search with no arg).
    tui.app_mut().begin_search();
    assert!(tui.app().is_editing_search());

    // Type a query.
    for c in ['f', 'o', 'o'] {
        tui.handle_event(Event::Key(KeyCode::Char(c)))
            .await
            .unwrap();
    }
    assert_eq!(tui.app().search_query_text(), "foo");
    // Input box still empty — the characters went to the query, not
    // the input.
    assert_eq!(tui.app().input(), "");

    // Backspace edits the query.
    tui.handle_event(Event::Key(KeyCode::Backspace))
        .await
        .unwrap();
    assert_eq!(tui.app().search_query_text(), "fo");

    // Enter commits: query stays, editing ends.
    tui.handle_event(Event::Key(KeyCode::Enter)).await.unwrap();
    assert!(!tui.app().is_editing_search());
    assert_eq!(tui.app().search_query_text(), "fo");

    // Escape clears.
    tui.handle_event(Event::Key(KeyCode::Escape)).await.unwrap();
    assert!(!tui.app().is_searching());
    assert_eq!(tui.app().search_query_text(), "");
}

/// With an active search targeting an early message, the chat widget
/// must scroll that message into view — not stay pinned to the live
/// bottom where the user happened to be before searching.
#[test]
fn test_search_scrolls_target_into_view() {
    use kod_tui::app::KodApp;
    use kod_tui::ui::ChatWidget;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let mut app = KodApp::new();
    for i in 0..40 {
        app.push_system_message(&format!("line-{i}-{}", "x".repeat(60)));
    }
    let n = app.set_search("line-0-x");
    assert_eq!(n, 1, "search should find exactly the first message");
    assert!(app.is_searching());

    let backend = TestBackend::new(80, 8);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|f| {
            ChatWidget::new().render(&app, f.area(), f.buffer_mut());
        })
        .unwrap();

    let visible: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    assert!(
        visible.contains("line-0-x"),
        "search target should be visible; got:\n{visible}"
    );
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

// ---------------------------------------------------------------------------
// Overlay widgets: approval dialog, help, question.
//
// These three widgets were at 0% line coverage when this block was added.
// Each renders conditionally on a `KodApp` accessor, so the interesting
// surface is: (a) the no-pending-state early return, (b) the populated
// render, (c) the branch that varies with the data (diff vs no diff,
// single-item vs multi-item batch, hint vs no hint).
// ---------------------------------------------------------------------------
mod widget_overlays {
    use super::*;
    use kod_tui::app::{PendingApproval, PendingApprovalBatch, PendingQuestion};
    use kod_tui::ui::{ApprovalWidget, HelpWidget, QuestionWidget};

    fn rect(w: u16, h: u16) -> ratatui::layout::Rect {
        ratatui::layout::Rect::new(0, 0, w, h)
    }

    fn approval_item(id: u64, tool: &str, summary: &str) -> PendingApproval {
        PendingApproval {
            id,
            tool_name: tool.to_string(),
            summary: summary.to_string(),
            diff: None,
        }
    }

    // --- ApprovalWidget -----------------------------------------------------

    #[test]
    fn approval_widget_renders_nothing_without_a_batch() {
        let app = KodApp::new();
        let area = rect(80, 24);
        let mut buffer = Buffer::empty(area);
        ApprovalWidget::new().render(&app, area, &mut buffer);
        let text = buffer_text(&buffer);
        assert!(
            !text.contains("approval required"),
            "no batch -> no dialog title, got: {text}"
        );
        assert!(
            !text.contains("tool:"),
            "no batch -> no dialog body, got: {text}"
        );
    }

    #[test]
    fn approval_widget_renders_single_item_with_diff() {
        let mut app = KodApp::new();
        app.set_pending_batch(PendingApprovalBatch {
            batch_id: 1,
            items: vec![PendingApproval {
                id: 7,
                tool_name: "write_file".into(),
                summary: "src/main.rs".into(),
                diff: Some("+ new line\n- old line\n context".into()),
            }],
            current: 0,
        });
        let area = rect(80, 24);
        let mut buffer = Buffer::empty(area);
        ApprovalWidget::new().render(&app, area, &mut buffer);
        let text = buffer_text(&buffer);
        assert!(text.contains("approval required"), "title, got: {text}");
        assert!(text.contains("write_file"), "tool name, got: {text}");
        assert!(text.contains("src/main.rs"), "summary, got: {text}");
        assert!(text.contains("+ new line"), "added diff line, got: {text}");
        assert!(text.contains("- old line"), "removed diff line, got: {text}");
        assert!(
            text.contains("never (session)"),
            "single-item legend, got: {text}"
        );
        // A single-item batch does NOT draw the multi-item legend.
        assert!(
            !text.contains("deny all remaining"),
            "no batch legend for one item, got: {text}"
        );
    }

    #[test]
    fn approval_widget_renders_multi_item_batch_header_and_legend() {
        let mut app = KodApp::new();
        app.set_pending_batch(PendingApprovalBatch {
            batch_id: 42,
            items: vec![
                approval_item(1, "write_file", "a.rs"),
                approval_item(2, "patch_file", "b.rs"),
                approval_item(3, "execute_command", "cargo check"),
            ],
            current: 1,
        });
        let area = rect(100, 30);
        let mut buffer = Buffer::empty(area);
        ApprovalWidget::new().render(&app, area, &mut buffer);
        let text = buffer_text(&buffer);
        assert!(text.contains("(2 of 3)"), "batch counter, got: {text}");
        assert!(text.contains("write_file"), "item 1 listed, got: {text}");
        assert!(text.contains("patch_file"), "item 2 listed, got: {text}");
        assert!(
            text.contains("execute_command"),
            "item 3 listed, got: {text}"
        );
        assert!(
            text.contains("deny all remaining"),
            "batch legend, got: {text}"
        );
    }

    #[test]
    fn approval_widget_no_diff_shows_placeholder() {
        let mut app = KodApp::new();
        app.set_pending_batch(PendingApprovalBatch {
            batch_id: 1,
            items: vec![approval_item(1, "write_file", "new_file.rs")],
            current: 0,
        });
        let area = rect(80, 24);
        let mut buffer = Buffer::empty(area);
        ApprovalWidget::new().render(&app, area, &mut buffer);
        let text = buffer_text(&buffer);
        assert!(
            text.contains("no diff"),
            "missing-diff placeholder, got: {text}"
        );
    }

    // --- HelpWidget ---------------------------------------------------------

    #[test]
    fn help_widget_renders_every_section_and_a_few_bindings() {
        let app = KodApp::new();
        let area = rect(100, 40);
        let mut buffer = Buffer::empty(area);
        HelpWidget::new().render(&app, area, &mut buffer);
        let text = buffer_text(&buffer);
        for section in ["help", "Modes", "Typing", "Chat", "Session"] {
            assert!(text.contains(section), "missing {section:?}, got: {text}");
        }
        assert!(text.contains("insert"), "insert description, got: {text}");
        assert!(text.contains("/retry"), "slash command listed, got: {text}");
    }

    #[test]
    fn help_widget_centered_clamps_to_area_and_centers_small() {
        let area = rect(40, 20);
        let big = HelpWidget::centered(area, 200, 200);
        assert_eq!((big.x, big.y, big.width, big.height), (0, 0, 40, 20));
        let small = HelpWidget::centered(area, 10, 6);
        assert_eq!((small.x, small.y, small.width, small.height), (15, 7, 10, 6));
    }

    // --- QuestionWidget -----------------------------------------------------

    #[test]
    fn question_widget_renders_nothing_without_a_question() {
        let app = KodApp::new();
        let area = rect(80, 24);
        let mut buffer = Buffer::empty(area);
        QuestionWidget::new().render(&app, area, &mut buffer);
        let text = buffer_text(&buffer);
        assert!(
            !text.contains("question"),
            "no question -> no dialog, got: {text}"
        );
    }

    #[test]
    fn question_widget_renders_prompt_hint_and_input() {
        let mut app = KodApp::new();
        app.set_pending_question(PendingQuestion {
            id: 3,
            question: "Which file?".into(),
            placeholder: Some("e.g. src/main.rs".into()),
        });
        // set_pending_question clears the input; fill it afterwards.
        *app.question_input_mut() = "src/lib.rs".into();

        let area = rect(80, 24);
        let mut buffer = Buffer::empty(area);
        QuestionWidget::new().render(&app, area, &mut buffer);
        let text = buffer_text(&buffer);
        assert!(text.contains("Which file?"), "prompt, got: {text}");
        assert!(text.contains("e.g. src/main.rs"), "hint, got: {text}");
        assert!(text.contains("src/lib.rs"), "typed input, got: {text}");
        assert!(text.contains("Enter submits"), "footer, got: {text}");
    }

    #[test]
    fn question_widget_omits_hint_when_absent() {
        let mut app = KodApp::new();
        app.set_pending_question(PendingQuestion {
            id: 4,
            question: "Continue?".into(),
            placeholder: None,
        });
        let area = rect(80, 24);
        let mut buffer = Buffer::empty(area);
        QuestionWidget::new().render(&app, area, &mut buffer);
        let text = buffer_text(&buffer);
        assert!(text.contains("Continue?"), "prompt, got: {text}");
        assert!(!text.contains("hint:"), "no hint line, got: {text}");
    }
}
