    //! Pins the tool-result half of the in-prompt redaction pass
    //! (Tier 1.3). Unlike the message path, `cap_rendered_result`
    //! takes the redactor as a parameter, so the test can drive it
    //! without needing the config flag or the engine.
    use super::*;

    #[test]
    fn cap_rendered_result_without_redactor_keeps_secrets() {
        let result = ToolResult::Success(serde_json::json!({
            "content": "API key: sk-abcdef1234567890ABCDEFGH",
        }));
        let out = cap_rendered_result(&result, 8_000, None);
        assert!(
            out.contains("sk-abcdef1234567890ABCDEFGH"),
            "no redactor: content must pass through",
        );
    }

    #[test]
    fn cap_rendered_result_with_redactor_strips_secrets() {
        let redactor = kod_types::redact::Redactor::default();
        let result = ToolResult::Success(serde_json::json!({
            "content": "API key: sk-abcdef1234567890ABCDEFGH",
        }));
        let out = cap_rendered_result(&result, 8_000, Some(&redactor));
        assert!(
            !out.contains("sk-abcdef1234567890ABCDEFGH"),
            "redactor must remove the key; got: {out}",
        );
        assert!(
            out.contains("[REDACTED:openai-key]"),
            "redactor must leave the marker; got: {out}",
        );
    }
