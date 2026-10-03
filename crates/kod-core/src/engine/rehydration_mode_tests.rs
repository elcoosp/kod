    use crate::engine::KodEngine;
    use crate::router::RouterConfig;
    use crate::session_log::{RehydrationMode, SessionEntry, SessionRecorder};

    fn engine_in(dir: &std::path::Path) -> KodEngine {
        let cfg = RouterConfig {
            embedder: None,
            skill_threshold: 0.3,
            context_window: 8192,
            short_term_capacity: 100,
            working_dir: dir.to_path_buf(),
            enable_memory: false,
            max_skills_per_query: 3,
        };
        KodEngine::new(cfg, dir.join("test.redb")).unwrap()
    }

    fn log_two_calls(path: &std::path::Path) {
        let rec = SessionRecorder::open(path.to_path_buf()).unwrap();
        for (i, name) in ["read_file", "grep"].iter().enumerate() {
            rec.record(&SessionEntry::ToolCall {
                timestamp_ms: 1_000 + i as u64,
                holder: String::new(),
                tool_name: name.to_string(),
                arguments: serde_json::json!({"path": format!("f{i}.rs")}),
                duration_ms: 5,
                result: serde_json::json!({"success": format!("out-{i}")}),
            })
            .unwrap();
        }
        rec.flush().unwrap();
    }

    #[tokio::test]
    async fn prose_mode_yields_user_messages() {
        let tmp = tempfile::TempDir::new().unwrap();
        let log = tmp.path().join("s.jsonl");
        log_two_calls(&log);
        let engine = engine_in(tmp.path());
        engine.start().await.unwrap();
        let n = engine
            .rehydrate_from_log_with("", &log, RehydrationMode::Prose)
            .await
            .unwrap();
        assert_eq!(n, 2);
        let history = engine.peek_transcript("", 4000).await;
        // Prose is User-role, so it survives the text renderer.
        assert!(history["tail"].as_str().unwrap().contains("[rehydrated]"));
    }

    #[tokio::test]
    async fn structured_mode_yields_tool_pairs() {
        let tmp = tempfile::TempDir::new().unwrap();
        let log = tmp.path().join("s.jsonl");
        log_two_calls(&log);
        let engine = engine_in(tmp.path());
        engine.start().await.unwrap();
        let n = engine
            .rehydrate_from_log_with("", &log, RehydrationMode::Structured)
            .await
            .unwrap();
        // Two calls => two assistant + two tool messages.
        assert_eq!(n, 4);
    }
