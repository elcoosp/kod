#![cfg(test)]
    //! Pins the invariant that `ground_prompt`'s tool inventory
    //! lands only in the volatile tail, never in the cacheable head
    //! (Tier 3.1). If a future change moved the inventory above the
    //! `## Volatile suffix` marker, every cached prompt would be
    //! invalidated by an unrelated tool registration, and the cache
    //! savings this whole design rests on would silently vanish.
    use super::*;

    fn head_of(prompt: &str) -> &str {
        const MARKER: &str = "## Volatile suffix";
        match prompt.find(MARKER) {
            Some(i) => &prompt[..i],
            None => prompt,
        }
    }

    fn make_tool(name: &str) -> ToolDefinition {
        ToolDefinition {
            id: kod_types::ToolId::new(),
            name: name.to_string(),
            description: format!("{name} test tool"),
            category: kod_types::ToolCategory::System,
            parameters_schema: serde_json::json!({}),
            permissions: kod_types::ToolPermissions::default(),
            trust_level: kod_types::trust::TrustLevel::default(),
            load_mode: Default::default(),
        }
    }

    #[tokio::test]
    async fn tool_inventory_only_touches_the_volatile_tail() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = RouterConfig {
            working_dir: tmp.path().to_path_buf(),
            enable_memory: false,
            ..RouterConfig::default()
        };
        let e = KodEngine::new(cfg, tmp.path().join("test.redb")).unwrap();

        // The router's rendered prompt ends at `## User Request`;
        // `ground_prompt` appends `## Environment` and `## Tool use`.
        // We feed a prompt that has the volatile marker so the split
        // is real, then vary the tool inventory.
        let base = "identity bits\n\n## Stable prefix (cacheable)\n\nrepo map\n\n## Volatile suffix (not cached)\n\nvolatile\n\n## User Request\n\nhi";

        let one = e.ground_prompt("session", base.to_string(), &[make_tool("alpha")]);
        let two = e.ground_prompt(
            "session",
            base.to_string(),
            &[make_tool("alpha"), make_tool("beta")],
        );

        // The head (everything before the volatile marker) must be
        // byte-identical.
        let h1 = head_of(&one);
        let h2 = head_of(&two);
        assert_eq!(h1, h2, "tool inventory change shifted the cacheable prefix",);
        // Sanity: the tail really does mention both tools.
        assert!(one.contains("alpha"), "alpha must be listed");
        assert!(two.contains("beta"), "beta must be listed");
    }
