    //! Tests for the auto-check injection in `run_tool_calls`.
    //!
    //! The feature is subtle: after a write_file succeeds, the engine
    //! runs the project's compiler and appends its diagnostics to the
    //! prompt the model sees on the next round. These tests drive a
    //! real tool round against a real tempfile workspace, so a
    //! regression in the injection shows up here rather than in
    //! production.

    use super::*;
    use kod_types::ToolCall;
    use tempfile::TempDir;

    /// A clean project: the write is followed by "Auto-check … is
    /// clean." in the prompt block.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_auto_check_clean_project() {
        let tmp = TempDir::new().unwrap();
        // Minimal Cargo project so CheckTool detects "cargo" and runs
        // `cargo check` — but the file has no errors, so the diagnostics
        // list is empty.
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src/lib.rs"), "pub fn ok() {}\n").unwrap();

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
        engine.set_auto_check(true);
        engine.start().await.unwrap();

        let calls = vec![ToolCall {
            id: None,
            tool_name: "write_file".to_string(),
            arguments: serde_json::json!({
                "path": "src/lib.rs",
                "content": "pub fn ok() {}\n// added a harmless comment\n"
            }),
        }];
        let round = engine.run_tool_calls(&calls, "test", None).await;
        assert_eq!(round.results.len(), 1);

        // The write succeeded (auto-check runs only on success).
        assert!(matches!(&round.results[0], ToolResult::Success(_)));

        // The prompt block should carry the auto-check section. We do
        // not assert "clean" text exactly because the cargo output
        // format may evolve; the presence of `## Auto-check` is the
        // contract.
        assert!(
            round.prompt_block.contains("## Auto-check"),
            "clean auto-check block missing: {}",
            round.prompt_block
        );
        assert!(
            round.prompt_block.contains("is clean"),
            "clean auto-check should say so: {}",
            round.prompt_block
        );
    }

    /// Auto-check is disabled by default: the block does not appear
    /// even when a write succeeds.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_auto_check_disabled_by_default() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src/lib.rs"), "pub fn ok() {}\n").unwrap();

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
        // Intentionally NOT calling set_auto_check(true).
        engine.start().await.unwrap();

        let calls = vec![ToolCall {
            id: None,
            tool_name: "write_file".to_string(),
            arguments: serde_json::json!({
                "path": "src/lib.rs",
                "content": "pub fn ok() {}\n"
            }),
        }];
        let round = engine.run_tool_calls(&calls, "test", None).await;
        assert!(
            !round.prompt_block.contains("## Auto-check"),
            "auto-check must not run when disabled: {}",
            round.prompt_block
        );
    }

    /// A round that writes two files must fall through to the
    /// compiler, which sees both. The per-file LSP path would answer
    /// for only one of them.
    ///
    /// This test works without rust-analyzer on PATH: the multi-file
    /// shape makes `lsp_eligible` false regardless, so the compiler
    /// path runs on every machine. What it proves is that the
    /// multi-file case never short-circuits through LSP.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_auto_check_multi_file_round_uses_compiler() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src/lib.rs"), "pub mod extra;\n").unwrap();
        std::fs::write(
            tmp.path().join("src/extra.rs"),
            "pub fn bad() -> u32 { 42 }\n",
        )
        .unwrap();

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
        engine.set_auto_check(true);
        engine.start().await.unwrap();

        // The round writes both files: lib.rs unchanged in spirit,
        // extra.rs introduces a type error. Only the compiler sees
        // both.
        let calls = vec![
            ToolCall {
                id: None,
                tool_name: "write_file".to_string(),
                arguments: serde_json::json!({
                    "path": "src/lib.rs",
                    "content": "pub mod extra;\n// harmless comment\n"
                }),
            },
            ToolCall {
                id: None,
                tool_name: "write_file".to_string(),
                arguments: serde_json::json!({
                    "path": "src/extra.rs",
                    "content": "pub fn bad() -> u32 { \"not a number\" }\n"
                }),
            },
        ];
        let round = engine.run_tool_calls(&calls, "test", None).await;
        assert_eq!(round.results.len(), 2);

        assert!(
            round.prompt_block.contains("## Auto-check"),
            "multi-file round should trigger auto-check: {}",
            round.prompt_block
        );
        assert!(
            round.prompt_block.contains("extra.rs"),
            "the compiler should name the file with the error: {}",
            round.prompt_block
        );
    }

    /// Auto-check with a non-write tool call must not trigger. The
    /// engine should not run `cargo check` for a `read_file`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_auto_check_skips_read_only_round() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src/lib.rs"), "pub fn ok() {}\n").unwrap();

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
        engine.set_auto_check(true);
        engine.start().await.unwrap();

        let calls = vec![ToolCall {
            id: None,
            tool_name: "read_file".to_string(),
            arguments: serde_json::json!({ "path": "src/lib.rs" }),
        }];
        let round = engine.run_tool_calls(&calls, "test", None).await;
        assert!(
            !round.prompt_block.contains("## Auto-check"),
            "read-only round must not trigger auto-check: {}",
            round.prompt_block
        );
    }
    /// A fixture with a pre-existing error. A write that introduces
    /// nothing new must produce a prompt that says so, not one that
    /// lists the pre-existing error as though the write caused it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_auto_check_reports_only_new_diagnostics() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        // `src/existing.rs` has a type error. `src/touched.rs` is clean.
        std::fs::write(
            tmp.path().join("src/lib.rs"),
            "pub mod existing;\npub mod touched;\n",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("src/existing.rs"),
            "pub fn bad() -> u32 { \"not a number\" }\n",
        )
        .unwrap();
        std::fs::write(tmp.path().join("src/touched.rs"), "pub fn ok() {}\n").unwrap();

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
        engine.set_auto_check(true);
        engine.start().await.unwrap();

        // Capture the baseline: the fixture's pre-existing error goes in.
        // Called explicitly so the test does not race the background task
        // that `start()` spawns.
        engine.refresh_check_baseline().await;
        let baseline = engine.check_baseline().await;
        assert!(
            baseline.as_ref().is_some_and(|b| !b.is_empty()),
            "baseline should have captured the pre-existing error"
        );

        // Now write only the clean file. The pre-existing error in
        // existing.rs must NOT be reported as new.
        let calls = vec![ToolCall {
            id: None,
            tool_name: "write_file".to_string(),
            arguments: serde_json::json!({
                "path": "src/touched.rs",
                "content": "pub fn ok() {}\n// harmless comment\n"
            }),
        }];
        let round = engine.run_tool_calls(&calls, "test", None).await;
        assert!(
            round.prompt_block.contains("## Auto-check"),
            "auto-check should have run: {}",
            round.prompt_block
        );
        assert!(
            !round.prompt_block.contains("NEW diagnostic"),
            "a write that introduces nothing must not report NEW diagnostics: {}",
            round.prompt_block
        );
        assert!(
            !round.prompt_block.contains("existing.rs"),
            "the pre-existing error must not appear in the auto-check block: {}",
            round.prompt_block
        );
        assert!(
            round.prompt_block.contains("no new errors"),
            "the block should say the write is clean: {}",
            round.prompt_block
        );
    }
