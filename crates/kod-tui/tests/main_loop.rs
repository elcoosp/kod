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

    // Esc no longer quits unconditionally — only 'q' does (or Esc while generating/help)
    tui.handle_event(Event::Key(KeyCode::Char('q')))
        .await
        .unwrap();

    assert!(tui.app().should_quit());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_escape_does_not_quit_in_normal() {
    let mut tui = TuiLoop::new();
    tui.handle_event(Event::Key(KeyCode::Escape)).await.unwrap();
    assert!(!tui.app().should_quit(), "Esc in normal mode must not quit");
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

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_slash_help_posts_message() {
    let mut tui = TuiLoop::new();

    tui.handle_event(Event::UserInput("/help".to_string()))
        .await
        .unwrap();

    // user echo + help text
    assert_eq!(tui.app().messages().len(), 2);
    assert!(tui.app().messages()[1].content.contains("/clear"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_slash_clear_empties_history() {
    let mut tui = TuiLoop::new();

    tui.handle_event(Event::UserInput("hello".to_string()))
        .await
        .unwrap();
    assert_eq!(tui.app().messages().len(), 1);

    // /clear is now queued for confirmation, so confirm with y.
    tui.handle_event(Event::UserInput("/clear".to_string()))
        .await
        .unwrap();
    assert!(!tui.app().messages().is_empty(), "/clear should ask first");
    tui.handle_event(Event::Key(KeyCode::Char('y')))
        .await
        .unwrap();
    assert!(tui.app().messages().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_slash_unknown_reports_error() {
    let mut tui = TuiLoop::new();

    tui.handle_event(Event::UserInput("/nope".to_string()))
        .await
        .unwrap();

    assert!(tui.app().messages()[1].content.contains("Unknown command"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_response_complete_lands_as_message() {
    let mut tui = TuiLoop::new();
    tui.app_mut().begin_generation();

    tui.handle_event(Event::ResponseChunk("hi ".to_string()))
        .await
        .unwrap();
    tui.handle_event(Event::ResponseComplete("".to_string()))
        .await
        .unwrap();

    assert!(!tui.app().is_generating());
    assert_eq!(tui.app().messages().len(), 1);
    assert!(tui.app().messages()[0].content.contains("hi"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[ignore = "needs a running OpenAI-compatible server matching the local kod config.
  KOD_TEST_MODEL=... KOD_TEST_DB=/tmp/kod-test.redb cargo test -p kod-tui -- --ignored"]
async fn test_live_prompt_roundtrip() {
    use kod_config::KodConfig;

    let config = KodConfig::load_default().expect("kod config must load");
    let model =
        std::env::var("KOD_TEST_MODEL").unwrap_or(config.llm.default_endpoint().model.clone());

    let mut tui = TuiLoop::new();
    tui.init_engine(Some(model))
        .await
        .expect("engine must init");
    assert!(!tui.app().model_name().is_empty());

    // Submit a prompt through the same path as the Enter key.
    tui.handle_event(Event::UserInput(
        "Reply with exactly: TUI_LIVE_OK".to_string(),
    ))
    .await
    .unwrap();
    assert!(tui.app().is_generating(), "thinking indicator must be on");

    // Drain events until the background generation completes.
    let mut seen = false;
    for _ in 0..600 {
        let event = tui.next_event().await;
        tui.handle_event(event).await.unwrap();
        if !tui.app().is_generating() {
            seen = true;
            break;
        }
    }
    assert!(seen, "generation must finish");
    let last = tui
        .app()
        .messages()
        .last()
        .expect("assistant reply must land");
    assert!(
        last.content.contains("TUI_LIVE_OK"),
        "unexpected reply: {}",
        last.content
    );
}

#[tokio::test]
async fn test_insert_mode_pageup_scrolls_conversation() {
    let mut tui = TuiLoop::new();
    tui.handle_event(Event::Key(KeyCode::Char('i')))
        .await
        .unwrap();
    assert_eq!(tui.app().input_mode(), &InputMode::Insert);
    tui.handle_event(Event::Key(KeyCode::PageUp)).await.unwrap();
    assert!(!tui.app().is_scrolled_to_bottom());
    tui.handle_event(Event::Key(KeyCode::End)).await.unwrap();
    assert!(tui.app().is_scrolled_to_bottom());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_slash_skills_lists_or_points_at_dirs() {
    let mut tui = TuiLoop::new();
    tui.handle_event(Event::UserInput("/skills".to_string()))
        .await
        .unwrap();
    let last = tui.app().messages().last().unwrap();
    assert!(last.content.contains("Skills (") || last.content.contains("~/.agents/skills"));
}

#[tokio::test]
async fn test_tool_events_surface_in_chat() {
    let mut tui = TuiLoop::new();
    tui.handle_event(Event::ToolStarted("read".to_string()))
        .await
        .unwrap();
    assert_eq!(tui.app().current_tool(), Some(&"read".to_string()));
    tui.handle_event(Event::ToolCompleted("read".to_string(), "ok".to_string()))
        .await
        .unwrap();
    assert_eq!(tui.app().current_tool(), None);
    assert!(
        tui.app()
            .messages()
            .last()
            .unwrap()
            .content
            .contains("[read]")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_enter_submits_slash_command() {
    // Regression: with the completion popup open, Enter used to only accept
    // the completion and swallow the submit — slash commands never ran.
    let mut tui = TuiLoop::new();
    tui.handle_event(Event::Key(KeyCode::Char('i')))
        .await
        .unwrap();
    for ch in ['/', 'h', 'e', 'l', 'p'] {
        tui.handle_event(Event::Key(KeyCode::Char(ch)))
            .await
            .unwrap();
    }
    assert!(tui.app().show_completions());
    tui.handle_event(Event::Key(KeyCode::Enter)).await.unwrap();
    // User echo + help output means the command actually dispatched.
    assert!(tui.app().messages().len() >= 2);
}
