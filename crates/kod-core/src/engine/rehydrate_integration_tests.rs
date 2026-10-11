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


// ---------------------------------------------------------------------------
// Cold revive (delta 11.2)
// ---------------------------------------------------------------------------

/// A cold revive against a session log whose `SessionInit` was written
/// by *this* process's tool surface must revive cleanly. This is the
/// positive control: `verify_cold_revive_surface` matches, and the
/// revive reports the same number of messages a bare
/// `rehydrate_from_log_for` would.
#[tokio::test]
async fn cold_revive_matches_after_the_same_session_init() {
    let temp = TempDir::new().unwrap();
    let log_path = temp.path().join("session.jsonl");
    let engine =
        KodEngine::new(fixture_config(temp.path()), temp.path().join("test.redb")).unwrap();

    // Install a recorder, start the engine: `start` writes the
    // `SessionInit` that carries the persisted tool surface.
    let recorder = std::sync::Arc::new(SessionRecorder::open(log_path.clone()).unwrap());
    engine.set_session_recorder(recorder.clone());
    engine.start().await.unwrap();

    // Record one tool call so the revive has something to rehydrate.
    recorder
        .record(&SessionEntry::ToolCall {
            timestamp_ms: 1_000,
            holder: String::new(),
            tool_name: "read_file".to_string(),
            arguments: serde_json::json!({"path": "src/lib.rs"}),
            duration_ms: 5,
            result: serde_json::json!({"success": "ok"}),
        })
        .unwrap();
    recorder.flush().unwrap();

    let outcome = engine
        .cold_revive_session(&log_path, "", false)
        .await
        .expect("cold revive against a matching surface must not error");
    match outcome {
        crate::ColdReviveOutcome::Revived {
            messages_appended,
        } => assert_eq!(
            messages_appended, 1,
            "one recorded tool call rehydrates to one prose message",
        ),
        other => panic!("expected Revived, got {other:?}"),
    }

    // The rehydrated turn must be visible to `render_history` — the
    // whole point of the Prose mode.
    let rendered = engine.render_history().await;
    assert!(
        rendered.contains("read_file"),
        "rehydrated call missing: {rendered}",
    );
    assert!(
        rendered.contains("ok"),
        "rehydrated result missing: {rendered}",
    );

    engine.shutdown().await.unwrap();
}

/// A cold revive against a log whose `SessionInit` names a tool the
/// current engine does not have must **refuse** — the safe default.
/// The refusal names the drift; a caller that decides the drift is
/// benign passes `force = true` and gets the revive.
#[tokio::test]
async fn cold_revive_refuses_drift_and_honours_force() {
    let temp = TempDir::new().unwrap();
    let log_path = temp.path().join("session.jsonl");

    // Write a `SessionInit` naming a tool that does not exist in this
    // process, plus one tool call whose result we can look for after
    // the forced revive.
    {
        let recorder = SessionRecorder::open(log_path.clone()).unwrap();
        recorder
            .record(&SessionEntry::SessionInit {
                timestamp_ms: 1_000,
                holder: String::new(),
                endpoint: "default".to_string(),
                model: "test".to_string(),
                tool_names: vec!["read_file".to_string(), "a_tool_from_the_future".to_string()],
                // Any hash: the surface fingerprint comparison dominates.
                system_prompt_hash: 0,
            })
            .unwrap();
        recorder
            .record(&SessionEntry::ToolCall {
                timestamp_ms: 1_001,
                holder: String::new(),
                tool_name: "read_file".to_string(),
                arguments: serde_json::json!({"path": "src/lib.rs"}),
                duration_ms: 5,
                result: serde_json::json!({"success": "forced-revive-payload"}),
            })
            .unwrap();
        recorder.flush().unwrap();
    }

    let engine =
        KodEngine::new(fixture_config(temp.path()), temp.path().join("test.redb")).unwrap();
    engine.start().await.unwrap();

    // Default: refused. The outcome names what drifted.
    let outcome = engine
        .cold_revive_session(&log_path, "", false)
        .await
        .expect("a drift refusal is an Ok, not an Err");
    match outcome {
        crate::ColdReviveOutcome::RefusedDrifted { missing, added } => {
            assert!(
                missing.iter().any(|n| n == "a_tool_from_the_future"),
                "missing list must name the persisted-but-absent tool: {missing:?}",
            );
            // `added` may be empty (the current surface is the engine's
            // default). We only require the drift was detected.
            let _ = added;
        }
        other => panic!("expected RefusedDrifted, got {other:?}"),
    }
    // Nothing was appended: the refusal must be a no-op on history.
    assert_eq!(
        engine.transcript_scoped_key_count_for_tests().await, 0,
        "a refused revive must not touch any transcript map",
    );

    // Forced: the drift is still reported by a *successful* revive.
    let outcome = engine
        .cold_revive_session(&log_path, "", true)
        .await
        .expect("a forced cold revive must not error");
    match outcome {
        crate::ColdReviveOutcome::Revived {
            messages_appended,
        } => assert_eq!(messages_appended, 1),
        other => panic!("expected Revived under force, got {other:?}"),
    }
    let rendered = engine.render_history().await;
    assert!(
        rendered.contains("forced-revive-payload"),
        "forced revive must have appended the recorded result: {rendered}",
    );

    engine.shutdown().await.unwrap();
}

/// A cold revive against a log with no `SessionInit` for the holder
/// returns `NoInitEntry` — the caller can fall back to a fresh-surface
/// revive, but the engine itself does nothing.
#[tokio::test]
async fn cold_revive_without_an_init_entry_reports_no_init() {
    let temp = TempDir::new().unwrap();
    let log_path = temp.path().join("session.jsonl");

    // Only a tool call — no `SessionInit`.
    {
        let recorder = SessionRecorder::open(log_path.clone()).unwrap();
        recorder
            .record(&SessionEntry::ToolCall {
                timestamp_ms: 1_000,
                holder: String::new(),
                tool_name: "read_file".to_string(),
                arguments: serde_json::json!({}),
                duration_ms: 1,
                result: serde_json::json!({"success": "x"}),
            })
            .unwrap();
        recorder.flush().unwrap();
    }

    let engine =
        KodEngine::new(fixture_config(temp.path()), temp.path().join("test.redb")).unwrap();
    engine.start().await.unwrap();

    let outcome = engine
        .cold_revive_session(&log_path, "", false)
        .await
        .expect("a log with no init entry is an Ok");
    assert!(matches!(outcome, crate::ColdReviveOutcome::NoInitEntry));

    engine.shutdown().await.unwrap();
}

/// A cold revive with a holder that has no `SessionInit` returns
/// `NoInitEntry` even when another holder's `SessionInit` is present.
#[tokio::test]
async fn cold_revive_for_an_unknown_holder_reports_no_init() {
    let temp = TempDir::new().unwrap();
    let log_path = temp.path().join("session.jsonl");

    {
        let recorder = SessionRecorder::open(log_path.clone()).unwrap();
        recorder
            .record(&SessionEntry::SessionInit {
                timestamp_ms: 1_000,
                holder: "swarm:agent-7".to_string(),
                endpoint: "default".to_string(),
                model: "test".to_string(),
                tool_names: vec!["read_file".to_string()],
                system_prompt_hash: 0,
            })
            .unwrap();
        recorder.flush().unwrap();
    }

    let engine =
        KodEngine::new(fixture_config(temp.path()), temp.path().join("test.redb")).unwrap();
    engine.start().await.unwrap();

    let outcome = engine
        .cold_revive_session(&log_path, "", false)
        .await
        .expect("unknown holder is an Ok(NoInitEntry)");
    assert!(matches!(outcome, crate::ColdReviveOutcome::NoInitEntry));

    engine.shutdown().await.unwrap();
}
