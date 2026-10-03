#![cfg(test)]
    use super::*;

    #[test]
    fn marker_round_trips() {
        let chunk = rate_limit_wait_marker(1200, 1, 2);
        assert_eq!(parse_rate_limit_wait(&chunk), Some((1200, 1, 2)));
    }
    #[test]
    fn non_marker_chunks_do_not_parse() {
        assert_eq!(parse_rate_limit_wait("hello"), None);
        assert_eq!(parse_rate_limit_wait(THINKING_MARKER), None);
        assert_eq!(
            parse_rate_limit_wait(&tool_start_marker("c1", "read_file")),
            None
        );
    }

    #[test]
    fn malformed_tail_degrades_to_defaults() {
        let chunk = format!("{RATE_LIMIT_WAIT_MARKER}60");
        assert_eq!(parse_rate_limit_wait(&chunk), Some((60, 1, 1)));
    }

    #[test]
    fn server_busy_marker_round_trips_distinct_from_rate_limit() {
        let chunk = server_busy_wait_marker(600, 1, 2);
        assert_eq!(parse_server_busy_wait(&chunk), Some((600, 1, 2)));
        // The two markers never collide.
        assert_eq!(parse_rate_limit_wait(&chunk), None);
        let rl = rate_limit_wait_marker(600, 1, 2);
        assert_eq!(parse_server_busy_wait(&rl), None);
        // The unified parser tags the kind.
        assert_eq!(parse_wait_marker(&chunk), Some((true, 600, 1, 2)));
        assert_eq!(parse_wait_marker(&rl), Some((false, 600, 1, 2)));
        assert_eq!(parse_wait_marker("hello"), None);
    }

    #[tokio::test]
    async fn server_busy_gets_four_waits_rate_limit_gets_two() {
        use tempfile::TempDir;
        let temp = TempDir::new().unwrap();
        let engine = KodEngine::new(RouterConfig::default(), temp.path().join("t.redb")).unwrap();
        engine.set_rate_limit_wait_budget(60);
        let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(16);
        let busy = KodError::ServerBusy { retry_after_secs: 1 };
        for attempt in 0..4 {
            let out = engine
                .rate_limit_retry_wait(&busy, attempt, "test-holder", &tx)
                .await
                .unwrap();
            assert!(out.is_some(), "busy attempt {attempt} should wait");
            let chunk = rx.recv().await.unwrap();
            assert_eq!(
                parse_server_busy_wait(&chunk),
                Some((1, attempt + 1, MAX_SERVER_BUSY_RETRIES))
            );
        }
        let out = engine
            .rate_limit_retry_wait(&busy, 4, "test-holder", &tx)
            .await
            .unwrap();
        assert!(out.is_none(), "busy attempt 4 must surface");
        let limited = KodError::RateLimited { retry_after_secs: 1 };
        for attempt in 0..2 {
            let out = engine
                .rate_limit_retry_wait(&limited, attempt, "test-holder", &tx)
                .await
                .unwrap();
            assert!(out.is_some(), "rate-limit attempt {attempt} should wait");
        }
        let out = engine
            .rate_limit_retry_wait(&limited, 2, "test-holder", &tx)
            .await
            .unwrap();
        assert!(out.is_none(), "rate-limit attempt 2 must surface");
    }

    #[test]
    fn other_tool_markers_do_not_collide_with_the_prefix() {
        // The tool markers share the `\0kod-` prefix; the rate-limit
        // parser must not claim them.
        assert_eq!(
            parse_rate_limit_wait(&format!("{TOOL_ARGS_MARKER}c1:ls\0")),
            None
        );
    }

    #[test]
    fn activity_marker_round_trips() {
        let chunk = activity_marker("saving memories…");
        assert_eq!(parse_activity_marker(&chunk), Some("saving memories…"));
    }

    #[test]
    fn activity_marker_rejects_prose_and_other_markers() {
        assert_eq!(parse_activity_marker("saving memories…"), None);
        assert_eq!(parse_activity_marker(THINKING_MARKER), None);
        assert_eq!(parse_activity_marker(&turn_marker(2)), None);
        assert_eq!(
            parse_activity_marker(&rate_limit_wait_marker(60, 1, 1)),
            None
        );
    }

    #[test]
    fn usage_marker_round_trips() {
        let chunk = usage_marker(12_000, 340);
        assert_eq!(parse_usage_marker(&chunk), Some((12_000, 340)));
    }

    #[test]
    fn usage_marker_rejects_prose_and_malformed_tails() {
        assert_eq!(parse_usage_marker("12000,340"), None);
        assert_eq!(parse_usage_marker(THINKING_MARKER), None);
        assert_eq!(parse_usage_marker(&turn_marker(2)), None);
        assert_eq!(parse_usage_marker("\0kod-usage:abc,1\0"), None);
        assert_eq!(parse_usage_marker("\0kod-usage:12\0"), None);
    }
