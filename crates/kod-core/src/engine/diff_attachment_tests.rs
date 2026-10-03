#![cfg(test)]
    use super::*;
    use kod_types::{ToolCall, ToolResult};
    use tempfile::TempDir;

    /// Regression: after `write_file` succeeds, the result must carry
    /// a `"diff"` field showing the before/after delta, so the TUI
    /// row can render what changed. The engine snapshots the file
    /// before the write; the diff is computed against that snapshot.
    #[tokio::test]
    async fn write_file_result_carries_diff() {
        let tmp = TempDir::new().unwrap();
        // Seed a file whose "before" content is known.
        std::fs::write(tmp.path().join("greet.txt"), "hello\n").unwrap();

        let db_path = tmp.path().join("test.redb");
        let cfg = RouterConfig {
            embedder: None,
            skill_threshold: 0.3,
            context_window: 8192,
            short_term_capacity: 100,
            working_dir: tmp.path().to_path_buf(),
            enable_memory: false,
            max_skills_per_query: 3,
        };
        let engine = KodEngine::new(cfg, db_path).unwrap();
        engine.start().await.unwrap();

        // Skip if the engine could not construct a checkpoint
        // manager — a container without a writable home directory
        // legitimately cannot attach diffs.
        if engine.checkpoints().is_none() {
            eprintln!("skipping: no checkpoint manager (no home directory)");
            return;
        }

        let calls = vec![ToolCall {
            id: None,
            tool_name: "write_file".to_string(),
            arguments: serde_json::json!({
                "path": "greet.txt",
                "content": "hello\nworld\n"
            }),
        }];
        let round = engine.run_tool_calls(&calls, "test", None).await;
        assert_eq!(round.results.len(), 1);
        match &round.results[0] {
            ToolResult::Success(v) => {
                let diff = v.get("diff").and_then(|d| d.as_str());
                assert!(
                    diff.is_some(),
                    "write_file result must carry a diff field, got: {v}"
                );
                let diff = diff.unwrap();
                assert!(
                    diff.contains("+world"),
                    "diff should show the added line: {diff}"
                );
                assert!(
                    diff.contains("greet.txt"),
                    "diff should name the file: {diff}"
                );
            }
            other => panic!("expected Success, got {other:?}"),
        }
    }

    /// The summary produced from that result names the file and
    /// shows the added line, not a "written N bytes" placeholder.
    #[test]
    fn write_file_summary_renders_diff() {
        let v = serde_json::json!({
            "path": "/tmp/greet.txt",
            "written": 12,
            "diff": "--- a/greet.txt\n+++ b/greet.txt\n@@ -1 +1,2 @@\n hello\n+world\n"
        });
        let rendered = summarize_tool_result("write_file", &ToolResult::Success(v));
        assert!(rendered.contains("greet.txt"), "got: {rendered}");
        assert!(rendered.contains("+world"), "got: {rendered}");
        assert!(
            !rendered.contains("written 12 bytes"),
            "old placeholder still present: {rendered}"
        );
    }

    /// An identical-content write shows "no change" rather than an
    /// empty diff (which would confuse a user expecting to see
    /// something).
    #[test]
    fn identical_write_summary_says_no_change() {
        let v = serde_json::json!({
            "path": "/tmp/same.txt",
            "written": 4,
            "diff": ""
        });
        let rendered = summarize_tool_result("write_file", &ToolResult::Success(v));
        assert!(
            rendered.contains("no change"),
            "expected a no-change notice: {rendered}"
        );
    }
