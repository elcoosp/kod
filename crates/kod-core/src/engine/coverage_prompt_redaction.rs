#![cfg(test)]
//! Pins the in-prompt redaction pass (Tier 1.3).
use super::*;
use crate::engine::state::redact_tool_call_arguments;

async fn engine() -> KodEngine {
    let tmp = tempfile::TempDir::new().unwrap();
    let cfg = RouterConfig {
        working_dir: tmp.path().to_path_buf(),
        enable_memory: false,
        ..RouterConfig::default()
    };
    let e = KodEngine::new(cfg, tmp.path().join("test.redb")).unwrap();
    std::mem::forget(tmp);
    e
}

#[tokio::test]
async fn redactor_is_a_noop_by_default() {
    let e = engine().await;
    let mut msgs = vec![kod_types::ChatMessage::text(
        kod_types::MessageId::new(),
        kod_types::MessageRole::User,
        "key is sk-abcdef1234567890ABCDEFGH".to_string(),
        time::OffsetDateTime::now_utc(),
    )];
    // `[security.redact] in_prompt = false` is the default, so
    // the caller's string is untouched.
    let n = e.redact_messages_for_prompt(&mut msgs);
    assert_eq!(n, 0);
    assert!(msgs[0].content.contains("sk-abcdef"));
}

#[tokio::test]
async fn tool_result_redaction_is_a_noop_by_default() {
    let e = engine().await;
    let s = e.redact_tool_result_for_prompt("token=sk-abcdef1234567890ABCDEFGH".to_string());
    assert!(s.contains("sk-abcdef"));
}

// --- `redact_argument_paths` wiring (session 11) ---
//
// The field was parsed and never read. `redact_tool_call_arguments`
// is the pure function the wire-in calls; these pin its behaviour
// directly, no engine and no config file needed.

fn tool_call_msg(name: &str, args: serde_json::Value) -> kod_types::ChatMessage {
    let mut m = kod_types::ChatMessage::text(
        kod_types::MessageId::new(),
        kod_types::MessageRole::Assistant,
        String::new(),
        time::OffsetDateTime::now_utc(),
    );
    m.tool_calls.push(kod_types::ToolCall::new(name, args));
    m
}

#[test]
fn argument_path_redaction_blanks_matching_call() {
    let mut msgs = vec![tool_call_msg(
        "write_file",
        serde_json::json!({"path": ".env", "content": "SECRET=abcdef"}),
    )];
    let n = redact_tool_call_arguments(
        &["**/.env".to_string()],
        &mut msgs,
        std::path::Path::new("/tmp/proj"),
    );
    assert_eq!(n, 1, "the matching call must be counted");
    let args = &msgs[0].tool_calls[0].arguments;
    assert!(
        !args.to_string().contains("SECRET"),
        "the secret-bearing field must be gone: {args}"
    );
    assert!(
        args.get("redacted").is_some(),
        "the replacement must be a valid JSON object with the marker: {args}"
    );
    // The call still names its tool — only the arguments are blanked.
    assert_eq!(msgs[0].tool_calls[0].tool_name, "write_file");
}

#[test]
fn argument_path_redaction_leaves_non_matching_calls_alone() {
    let mut msgs = vec![tool_call_msg(
        "write_file",
        serde_json::json!({"path": "src/main.rs", "content": "fn main() {}"}),
    )];
    let n = redact_tool_call_arguments(
        &["**/.env".to_string()],
        &mut msgs,
        std::path::Path::new("/tmp/proj"),
    );
    assert_eq!(n, 0);
    assert_eq!(
        msgs[0].tool_calls[0].arguments,
        serde_json::json!({"path": "src/main.rs", "content": "fn main() {}"}),
    );
}

#[test]
fn argument_path_redaction_is_a_noop_with_empty_patterns() {
    let mut msgs = vec![tool_call_msg(
        "write_file",
        serde_json::json!({"path": ".env", "content": "SECRET"}),
    )];
    let n = redact_tool_call_arguments(&[], &mut msgs, std::path::Path::new("/tmp/proj"));
    assert_eq!(n, 0);
    assert!(
        msgs[0].tool_calls[0]
            .arguments
            .to_string()
            .contains("SECRET")
    );
}

#[test]
fn argument_path_redaction_only_touches_its_own_message() {
    let mut msgs = vec![
        kod_types::ChatMessage::text(
            kod_types::MessageId::new(),
            kod_types::MessageRole::User,
            "hello".to_string(),
            time::OffsetDateTime::now_utc(),
        ),
        tool_call_msg("write_file", serde_json::json!({"path": ".env"})),
    ];
    let n = redact_tool_call_arguments(
        &["**/.env".to_string()],
        &mut msgs,
        std::path::Path::new("/tmp/proj"),
    );
    assert_eq!(n, 1);
    assert_eq!(msgs[0].content, "hello");
}

// End-to-end: the config field reaches the pass. nextest runs each
// test in its own process, so `KOD_CONFIG_DIR` is safe to set here
// even though `load_cached`'s cache is a process-global `OnceLock`.
#[tokio::test]
async fn e2e_argument_redaction_via_config() {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("config.toml"),
        "[security.redact]\nredact_argument_paths = [\"**/.env\"]\n",
    )
    .unwrap();

    // The process reads the config on the first `load_cached`, which
    // `KodEngine::new` triggers. Set before building the engine.
    // SAFETY: single-threaded test process under nextest.
    unsafe { std::env::set_var("KOD_CONFIG_DIR", dir.path()) };

    let e = engine().await;
    let mut msgs = vec![tool_call_msg(
        "write_file",
        serde_json::json!({"path": ".env", "content": "SECRET=abcdef"}),
    )];
    let n = e.redact_messages_for_prompt(&mut msgs);
    assert_eq!(n, 1, "the configured pattern must reach the pass");
    assert!(
        !msgs[0].tool_calls[0]
            .arguments
            .to_string()
            .contains("SECRET"),
        "the secret must be gone from the wire-visible arguments"
    );

    // SAFETY: single-threaded test process under nextest.
    unsafe { std::env::remove_var("KOD_CONFIG_DIR") };
}
