#![cfg(test)]
    //! Pins the shape of `KodEngine::apply_retry_adjustment`
    //! (Tier 3.3). The function is pure: it mutates two locals and
    //! returns whether the strategy could help. A regression either
    //! changes the resulting options/messages or the return
    //! sentinel, both of which a caller relies on.
    use super::*;

    fn empty_opts() -> GenerationOptions {
        GenerationOptions::default()
    }

    fn empty_msgs() -> Vec<kod_types::ChatMessage> {
        Vec::new()
    }

    #[test]
    fn lower_temp_halves_the_temperature() {
        use kod_core_routing::retry_strategy::RetryAction;
        let mut o = empty_opts();
        o.temperature = Some(0.8);
        let mut m = empty_msgs();
        let ok =
            KodEngine::apply_retry_adjustment(RetryAction::SameEndpointLowerTemp, &mut o, &mut m);
        assert!(ok);
        assert!((o.temperature.unwrap() - 0.4).abs() < 1e-6);
        assert!(m.is_empty(), "no message change expected");
    }

    #[test]
    fn lower_temp_defaults_when_unset() {
        use kod_core_routing::retry_strategy::RetryAction;
        let mut o = empty_opts();
        o.temperature = None;
        let mut m = empty_msgs();
        let ok =
            KodEngine::apply_retry_adjustment(RetryAction::SameEndpointLowerTemp, &mut o, &mut m);
        assert!(ok);
        // Default 0.7 halved is 0.35.
        assert!((o.temperature.unwrap() - 0.35).abs() < 1e-6);
    }

    #[test]
    fn reinject_tools_appends_a_system_nudge() {
        use kod_core_routing::retry_strategy::RetryAction;
        let mut o = empty_opts();
        let mut m = empty_msgs();
        let ok = KodEngine::apply_retry_adjustment(RetryAction::ReinjectTools, &mut o, &mut m);
        assert!(ok);
        assert_eq!(m.len(), 1);
        assert!(matches!(m[0].role, kod_types::MessageRole::System));
        assert!(m[0].content.contains("tool"));
    }

    #[test]
    fn constrained_appends_a_json_nudge() {
        use kod_core_routing::retry_strategy::RetryAction;
        let mut o = empty_opts();
        let mut m = empty_msgs();
        let ok =
            KodEngine::apply_retry_adjustment(RetryAction::SameEndpointConstrained, &mut o, &mut m);
        assert!(ok);
        assert_eq!(m.len(), 1);
        assert!(m[0].content.to_lowercase().contains("json"));
    }

    #[test]
    fn shrink_history_refuses_short_conversations() {
        use kod_core_routing::retry_strategy::RetryAction;
        let mut o = empty_opts();
        let mut m = vec![
            kod_types::ChatMessage::text(
                kod_types::MessageId::new(),
                kod_types::MessageRole::User,
                String::from("a"),
                time::OffsetDateTime::now_utc(),
            ),
            kod_types::ChatMessage::text(
                kod_types::MessageId::new(),
                kod_types::MessageRole::Assistant,
                String::from("b"),
                time::OffsetDateTime::now_utc(),
            ),
        ];
        let ok = KodEngine::apply_retry_adjustment(RetryAction::ShrinkHistory, &mut o, &mut m);
        assert!(!ok, "less than 4 messages cannot be shrunk");
        assert_eq!(m.len(), 2, "messages unchanged on refusal");
    }

    #[test]
    fn shrink_history_drops_oldest_half() {
        use kod_core_routing::retry_strategy::RetryAction;
        let mut o = empty_opts();
        let mut m: Vec<kod_types::ChatMessage> = (0..6)
            .map(|i| {
                kod_types::ChatMessage::text(
                    kod_types::MessageId::new(),
                    kod_types::MessageRole::User,
                    format!("m{i}"),
                    time::OffsetDateTime::now_utc(),
                )
            })
            .collect();
        let ok = KodEngine::apply_retry_adjustment(RetryAction::ShrinkHistory, &mut o, &mut m);
        assert!(ok);
        assert_eq!(m.len(), 3, "kept the newer half");
        assert!(m[0].content.ends_with('3'));
    }

    #[test]
    fn next_endpoint_is_not_handled_here() {
        use kod_core_routing::retry_strategy::RetryAction;
        let mut o = empty_opts();
        let mut m = empty_msgs();
        assert!(!KodEngine::apply_retry_adjustment(
            RetryAction::NextEndpoint,
            &mut o,
            &mut m,
        ));
        assert!(!KodEngine::apply_retry_adjustment(
            RetryAction::SameEndpointBackoff,
            &mut o,
            &mut m,
        ));
        assert!(!KodEngine::apply_retry_adjustment(
            RetryAction::NoRetry,
            &mut o,
            &mut m,
        ));
    }
