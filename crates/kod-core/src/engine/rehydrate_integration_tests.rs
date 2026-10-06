#![cfg(test)]
use super::*;
use crate::router::RouterConfig;
use kod_core_state::session_log::{SessionEntry, SessionRecorder};
use tempfile::TempDir;

fn fixture_config(dir: &std::path::Path) -> RouterConfig {
    RouterConfig {
        embedder: None,
        skill_threshold: 0.3,
        context_window: 8192,
        short_term_capacity: 100,
        working_dir: dir.to_path_buf(),
        enable_memory: false,
        max_skills_per_query: 3,
    }
}

#[tokio::test]
async fn rehydrate_from_log_for_appends_prose_turns() {
    let temp = TempDir::new().unwrap();
    let log_path = temp.path().join("session.jsonl");
    let engine =
        KodEngine::new(fixture_config(temp.path()), temp.path().join("test.redb")).unwrap();
    engine.start().await.unwrap();

    // Two tool calls recorded against holder "session".
    let recorder = SessionRecorder::open(log_path.clone()).unwrap();
    for (i, name) in ["read_file", "grep"].iter().enumerate() {
        recorder
            .record(&SessionEntry::ToolCall {
                timestamp_ms: 1_000 + i as u64,
                holder: String::new(),
                tool_name: name.to_string(),
                arguments: serde_json::json!({"path": format!("f{i}.rs")}),
                duration_ms: 5,
                result: serde_json::json!({"success": format!("result-{i}")}),
            })
            .unwrap();
    }
    recorder.flush().unwrap();
    drop(recorder);

    let count = engine.rehydrate_from_log_for("", &log_path).await.unwrap();
    assert_eq!(count, 2, "two tool calls => two prose messages");

    // The prose must be visible to render_history. This is the
    // property the structured form does NOT have: Tool-role rows
    // are filtered out of the rendered prompt.
    let rendered = engine.render_history().await;
    assert!(
        rendered.contains("[rehydrated]"),
        "rehydrated turn missing from render_history: {rendered}",
    );
    assert!(
        rendered.contains("read_file"),
        "rehydrated call name missing: {rendered}",
    );
    assert!(
        rendered.contains("result-0"),
        "rehydrated result missing: {rendered}",
    );

    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn rehydrate_from_log_for_ignores_other_holders() {
    let temp = TempDir::new().unwrap();
    let log_path = temp.path().join("session.jsonl");
    let engine =
        KodEngine::new(fixture_config(temp.path()), temp.path().join("test.redb")).unwrap();
    engine.start().await.unwrap();

    let recorder = SessionRecorder::open(log_path.clone()).unwrap();
    recorder
        .record(&SessionEntry::ToolCall {
            timestamp_ms: 1_000,
            holder: "swarm:agent-9".to_string(),
            tool_name: "read_file".to_string(),
            arguments: serde_json::json!({}),
            duration_ms: 5,
            result: serde_json::json!({"success": "elsewhere"}),
        })
        .unwrap();
    recorder.flush().unwrap();
    drop(recorder);

    let count = engine.rehydrate_from_log_for("", &log_path).await.unwrap();
    assert_eq!(count, 0, "entries for another holder must be ignored");

    let rendered = engine.render_history().await;
    assert!(!rendered.contains("elsewhere"));

    engine.shutdown().await.unwrap();
}

#[tokio::test]
async fn rehydrate_from_log_for_on_missing_file_is_a_no_op() {
    let temp = TempDir::new().unwrap();
    let engine =
        KodEngine::new(fixture_config(temp.path()), temp.path().join("test.redb")).unwrap();
    engine.start().await.unwrap();

    let missing = temp.path().join("does-not-exist.jsonl");
    // read_session propagates io::Error for a missing file; the
    // contract here is "surface the error, do not touch history".
    let result = engine.rehydrate_from_log_for("", &missing).await;
    assert!(result.is_err(), "missing log must surface an error");

    engine.shutdown().await.unwrap();
}
