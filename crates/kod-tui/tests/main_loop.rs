use kod_tui::app::{AppMode, InputMode};
use kod_tui::event::{Event, KeyCode};
use kod_tui::main_loop::TuiLoop;

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_tui_loop_creation() {
    let tui = TuiLoop::new();
    assert!(tui.is_initialized());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_tui_event_processing() {
    let mut tui = TuiLoop::new();

    tui.handle_event(Event::Key(KeyCode::Char('i')))
        .await
        .unwrap();
    tui.handle_event(Event::Key(KeyCode::Char('h')))
        .await
        .unwrap();
    tui.handle_event(Event::Key(KeyCode::Char('i')))
        .await
        .unwrap();

    assert_eq!(tui.app().input_mode(), &InputMode::Insert);
    assert_eq!(tui.app().input(), "hi");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_tui_input_submission() {
    let mut tui = TuiLoop::new();

    tui.handle_event(Event::Key(KeyCode::Char('i')))
        .await
        .unwrap();
    tui.handle_event(Event::Key(KeyCode::Char('t')))
        .await
        .unwrap();
    tui.handle_event(Event::Key(KeyCode::Char('e')))
        .await
        .unwrap();
    tui.handle_event(Event::Key(KeyCode::Char('s')))
        .await
        .unwrap();
    tui.handle_event(Event::Key(KeyCode::Char('t')))
        .await
        .unwrap();

    tui.handle_event(Event::Key(KeyCode::Enter)).await.unwrap();

    assert_eq!(tui.app().messages().len(), 1);
    assert!(tui.app().messages()[0].content.contains("test"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_tui_quit() {
    let mut tui = TuiLoop::new();

    tui.handle_event(Event::Key(KeyCode::Escape)).await.unwrap();

    assert!(tui.app().should_quit());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_tui_mode_switching() {
    let mut tui = TuiLoop::new();

    assert_eq!(tui.app().mode(), &AppMode::Normal);

    tui.handle_event(Event::Key(KeyCode::Tab)).await.unwrap();
    assert_eq!(tui.app().mode(), &AppMode::AgentPanel);

    tui.handle_event(Event::Key(KeyCode::Tab)).await.unwrap();
    assert_eq!(tui.app().mode(), &AppMode::Normal);
}
