use kod_tui::app::{AppMode, InputMode};
use kod_tui::event::Event;
use kod_tui::main_loop::TuiLoop;

fn make_key_event(code: crossterm::event::KeyCode) -> crossterm::event::KeyEvent {
    crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::empty())
}

#[tokio::test]
async fn test_tui_creation() {
    let tui = TuiLoop::new();
    assert!(!tui.should_quit());
    assert!(tui.is_running());
    assert_eq!(tui.app().mode(), &AppMode::Normal);
}

#[tokio::test]
async fn test_tui_event_processing() {
    let mut tui = TuiLoop::new();
    tui.handle_event(Event::Key(make_key_event(crossterm::event::KeyCode::Char('i'))))
        .await
        .unwrap();
    assert_eq!(tui.app().input_mode(), &InputMode::Insert);
}

#[tokio::test]
async fn test_tui_quit() {
    let mut tui = TuiLoop::new();
    tui.handle_event(Event::Key(make_key_event(crossterm::event::KeyCode::Esc)))
        .await
        .unwrap();
    assert!(tui.app().should_quit());
    assert!(!tui.is_running());
}

#[tokio::test]
async fn test_tui_mode_switching() {
    let mut tui = TuiLoop::new();

    // Normal -> AgentPanel
    tui.handle_event(Event::Key(make_key_event(crossterm::event::KeyCode::Tab)))
        .await
        .unwrap();
    assert_eq!(tui.app().mode(), &AppMode::AgentPanel);

    // AgentPanel -> Normal
    tui.handle_event(Event::Key(make_key_event(crossterm::event::KeyCode::Tab)))
        .await
        .unwrap();
    assert_eq!(tui.app().mode(), &AppMode::Normal);
}

#[tokio::test]
async fn test_tui_input_submission() {
    let mut tui = TuiLoop::new();

    // Enter insert mode
    tui.handle_event(Event::Key(make_key_event(crossterm::event::KeyCode::Char('i'))))
        .await
        .unwrap();
    assert_eq!(tui.app().input_mode(), &InputMode::Insert);

    // Type "test"
    for c in ['t', 'e', 's', 't'] {
        tui.handle_event(Event::Key(make_key_event(crossterm::event::KeyCode::Char(
            c,
        ))))
        .await
        .unwrap();
    }

    // Submit with Enter
    tui.handle_event(Event::Key(make_key_event(crossterm::event::KeyCode::Enter)))
        .await
        .unwrap();

    // Should have one message
    assert_eq!(tui.app().messages().len(), 1);
    let msg = &tui.app().messages()[0];
    assert_eq!(msg.role, kod_types::MessageRole::User);
    assert_eq!(msg.content, "test");
}

#[tokio::test]
async fn test_tui_normal_mode_keys() {
    let mut tui = TuiLoop::new();

    // Press 'a' to switch to AgentPanel
    tui.handle_event(Event::Key(make_key_event(crossterm::event::KeyCode::Char('a'))))
        .await
        .unwrap();
    assert_eq!(tui.app().mode(), &AppMode::AgentPanel);

    // Press Tab to go back to Normal
    tui.handle_event(Event::Key(make_key_event(crossterm::event::KeyCode::Tab)))
        .await
        .unwrap();
    assert_eq!(tui.app().mode(), &AppMode::Normal);
}

#[tokio::test]
async fn test_tui_backspace() {
    let mut tui = TuiLoop::new();

    // Enter insert mode
    tui.handle_event(Event::Key(make_key_event(crossterm::event::KeyCode::Char('i'))))
        .await
        .unwrap();

    // Type "hello"
    for c in ['h', 'e', 'l', 'l', 'o'] {
        tui.handle_event(Event::Key(make_key_event(crossterm::event::KeyCode::Char(
            c,
        ))))
        .await
        .unwrap();
    }
    assert_eq!(tui.app().current_input(), "hello");

    // Backspace to remove one char
    tui.handle_event(Event::Key(make_key_event(
        crossterm::event::KeyCode::Backspace,
    )))
    .await
    .unwrap();
    assert_eq!(tui.app().current_input(), "hell");

    // Esc to exit insert mode
    tui.handle_event(Event::Key(make_key_event(crossterm::event::KeyCode::Esc)))
        .await
        .unwrap();
    assert_eq!(tui.app().input_mode(), &InputMode::Normal);
}
