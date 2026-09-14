use kod_tui::app::{AppMode, InputMode, KodApp, Message};
use kod_types::{MessageId, MessageRole};

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
        sequence: 0,
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

    app.add_agent(
        "architect",
        vec!["planning".to_string(), "research".to_string()],
    );
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

    assert!(
        app.messages()
            .iter()
            .any(|m| { m.role == MessageRole::Tool && m.content.contains("read_file") })
    );
}

#[test]
fn test_streaming_response() {
    let mut app = KodApp::new();

    app.start_response_stream();

    app.add_response_chunk("Hello");
    app.add_response_chunk(" world");

    assert_eq!(app.current_response(), "Hello world");

    app.complete_response();

    assert!(
        app.messages()
            .iter()
            .any(|m| { m.role == MessageRole::Assistant && m.content == "Hello world" })
    );

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
            sequence: 0,
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
        sequence: 0,
    });

    let json = serde_json::to_string(&app.messages()).unwrap();
    let parsed: Vec<Message> = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].content, "test");
}

#[test]
fn test_assistant_message_trims_blank_lines() {
    let mut app = KodApp::new();
    app.push_assistant_message("\n\n\nhello\n\n");
    assert_eq!(app.messages().len(), 1);
    assert_eq!(app.messages()[0].content, "hello");
}

#[test]
fn test_context_label_and_manual_compact() {
    let mut app = KodApp::new();
    assert!(app.context_label().contains("ctx"));
    // 30 messages -> compact_now keeps the newest 20 + 1 notice.
    for i in 0..30 {
        app.push_assistant_message(&format!("msg {}", i));
    }
    app.compact_now();
    assert!(app.messages().len() <= 22);
    assert!(app.messages().last().unwrap().content.contains("Compacted"));
}

#[test]
fn test_path_completion_lists_matching_files() {
    let dir = std::env::temp_dir().join(format!("kod-path-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("alpha.txt"), "a").unwrap();
    std::fs::write(dir.join("alpine.txt"), "a").unwrap();
    std::fs::write(dir.join("beta.txt"), "b").unwrap();

    let mut app = KodApp::new();
    app.set_input_mode(InputMode::Insert);
    let token = format!("{}/alp", dir.display());
    for ch in format!("read {token}").chars() {
        app.add_char(ch);
    }
    let candidates = app.path_candidates();
    assert_eq!(candidates.len(), 2);
    assert!(candidates.iter().all(|c| c.ends_with(".txt")));

    // Accepting replaces only the path token, keeping `read ` (plus a
    // trailing space so the user can keep typing).
    app.accept_completion();
    assert!(app.input().starts_with("read "));
    assert!(app.input().trim_end().ends_with(".txt"));

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn test_no_path_completion_for_plain_words() {
    let mut app = KodApp::new();
    app.set_input_mode(InputMode::Insert);
    for ch in "hello world".chars() {
        app.add_char(ch);
    }
    assert!(app.path_candidates().is_empty());
    assert!(!app.show_completions());
}


/// `KodApp::save_session` + `load_session` are the only persistence
/// between TUI runs. Regression: both existed but neither was called
/// from TuiLoop, so a restart silently lost the whole chat and the
/// engine's transcript started empty every time.
///
/// Uses KOD_TUI_STATE_DIR so the test never touches the user's real
/// ~/.kod directory. The env var is process-global, so this test must
/// not run in parallel with any other test that relies on the same
/// state dir — it sets a per-test unique path under the OS temp dir,
/// and the other session test below uses a different dir.
#[test]
fn test_session_persistence_roundtrip() {
    use kod_tui::app::{KodApp, Message};
    use kod_types::{MessageId, MessageMetadata, MessageRole};

    let tmp = std::env::temp_dir().join(format!(
        "kod-tui-session-test-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    // SAFETY: tests that touch KOD_TUI_STATE_DIR are serialized below
    // via a shared mutex (see `session_state_dir_lock`).
    let _guard = session_state_dir_lock();
    unsafe { std::env::set_var("KOD_TUI_STATE_DIR", &tmp) };

    // Build a session with a user prompt and an assistant reply.
    let mut app = KodApp::new();
    app.add_message(Message {
        id: MessageId::new(),
        role: MessageRole::User,
        content: "hello from a test".to_string(),
        timestamp: chrono::Utc::now(),
        metadata: MessageMetadata::default(),
        sequence: 0,
    });
    app.add_message(Message {
        id: MessageId::new(),
        role: MessageRole::Assistant,
        content: "hi, I am an assistant".to_string(),
        timestamp: chrono::Utc::now(),
        metadata: MessageMetadata::default(),
        sequence: 0,
    });
    app.save_session();

    // A fresh app loading the same state dir must see the same messages
    // in the same order.
    let mut restored = KodApp::new();
    let n = restored.load_session();
    assert_eq!(n, 2, "expected 2 restored messages");
    let roles: Vec<_> = restored.messages().iter().map(|m| m.role.clone()).collect();
    assert_eq!(roles, vec![MessageRole::User, MessageRole::Assistant]);
    assert_eq!(restored.messages()[0].content, "hello from a test");
    assert_eq!(restored.messages()[1].content, "hi, I am an assistant");
    // Sequences must be monotonic so new messages sort after restored ones.
    assert!(restored.messages()[0].sequence < restored.messages()[1].sequence);

    unsafe { std::env::remove_var("KOD_TUI_STATE_DIR") };
    let _ = std::fs::remove_dir_all(&tmp);
}

/// Serializes tests that mutate KOD_TUI_STATE_DIR (process-global).
/// Rust runs unit tests in parallel by default; the session tests must
/// not race each other for the same env var.
fn session_state_dir_lock() -> std::sync::MutexGuard<'static, ()> {
    use std::sync::{Mutex, OnceLock};
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}


/// Friendly-error coverage: each branch of KodApp::friendly_error must
/// fire on the phrasing OpenAI-compatible servers actually produce,
/// and the raw error text must always be preserved.
#[test]
fn test_friendly_error_advice() {
    use kod_tui::app::KodApp;

    // 1. Model not found — advice should name `ollama pull`.
    let m = KodApp::friendly_error(
        "provider error: model `codellama:13b` not found",
        0,
    );
    assert!(m.contains("Error:"), "raw text preserved: {m}");
    assert!(m.contains("ollama pull"), "model advice missing: {m}");

    // 2. Context length — advice should name /compact.
    let m = KodApp::friendly_error(
        "400 Bad Request: maximum context length is 8192 tokens",
        0,
    );
    assert!(m.contains("/compact"), "context advice missing: {m}");

    // 3. Malformed tool call — advice should mention tool calling.
    let m = KodApp::friendly_error(
        "provider error: invalid tool call: expected value at line 1",
        0,
    );
    assert!(
        m.contains("tool call") && m.contains("tool calling"),
        "tool-parse advice missing: {m}"
    );

    // 4. Connectivity — advice should mention ollama serve.
    let m = KodApp::friendly_error(
        "connection refused: 127.0.0.1:11434",
        0,
    );
    assert!(m.contains("ollama serve"), "connection advice missing: {m}");

    // 5. Auth — advice should mention api_key.
    let m = KodApp::friendly_error("401 Unauthorized", 0);
    assert!(m.contains("api_key"), "auth advice missing: {m}");

    // 6. Timeout — advice should mention /retry.
    let m = KodApp::friendly_error("request timed out after 300s", 0);
    assert!(m.contains("/retry"), "timeout advice missing: {m}");

    // 7. Unknown error — raw text preserved, no advice appended.
    let m = KodApp::friendly_error("something else went wrong", 0);
    assert_eq!(m, "Error: something else went wrong");

    // 8. fail_count >= 2 appends the offline-mode footer to any branch.
    let m = KodApp::friendly_error("connection refused", 3);
    assert!(m.contains("offline mode"), "offline footer missing: {m}");

    // 9. Specific-branch precedence: a message containing both
    // "model not found" and "404" should pick the model-not-found
    // advice, not the endpoint-not-found one.
    let m = KodApp::friendly_error(
        "404 Not Found: model not found in registry",
        0,
    );
    assert!(
        m.contains("ollama pull"),
        "model-not-found should win over 404: {m}"
    );
}
