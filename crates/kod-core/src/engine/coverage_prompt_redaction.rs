#![cfg(test)]
    //! Pins the in-prompt redaction pass (Tier 1.3).
    use super::*;

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
