#![cfg(test)]
    //! Pins the engine's decision-log accessors (Tier 3.4).
    use super::*;
    use crate::decisions::{DecisionAuthor, DecisionKind, DecisionLog};

    async fn engine() -> KodEngine {
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = RouterConfig {
            working_dir: tmp.path().to_path_buf(),
            enable_memory: false,
            ..RouterConfig::default()
        };
        let e = KodEngine::new(cfg, tmp.path().join("test.redb")).unwrap();
        // Keep the TempDir alive by leaking it — tests are short.
        std::mem::forget(tmp);
        e
    }

    #[tokio::test]
    async fn empty_log_is_the_default() {
        let e = engine().await;
        let log = e.decisions_for("session").await;
        assert!(log.entries.is_empty());
    }

    #[tokio::test]
    async fn add_then_read() {
        let e = engine().await;
        let id = e
            .add_decision(
                "session",
                1,
                DecisionKind::UserPreference,
                "prefer tabs".into(),
                DecisionAuthor::User,
            )
            .await;
        assert_eq!(id, 0);
        let log = e.decisions_for("session").await;
        assert_eq!(log.entries.len(), 1);
        assert_eq!(log.entries[0].text, "prefer tabs");
    }

    #[tokio::test]
    async fn drop_by_id() {
        let e = engine().await;
        let id = e
            .add_decision(
                "session",
                1,
                DecisionKind::Approach,
                "use similar".into(),
                DecisionAuthor::Assistant,
            )
            .await;
        assert!(e.drop_decision("session", id).await);
        assert!(!e.drop_decision("session", 999).await);
        assert!(e.decisions_for("session").await.entries.is_empty());
    }

    #[tokio::test]
    async fn set_decision_log_replaces() {
        let e = engine().await;
        e.add_decision(
            "session",
            1,
            DecisionKind::Other,
            "will be replaced".into(),
            DecisionAuthor::User,
        )
        .await;
        let mut fresh = DecisionLog::new();
        fresh.push(
            2,
            DecisionKind::Constraint,
            "no new deps".into(),
            DecisionAuthor::User,
        );
        e.set_decision_log("session", fresh).await;
        let log = e.decisions_for("session").await;
        assert_eq!(log.entries.len(), 1);
        assert_eq!(log.entries[0].text, "no new deps");
    }

    #[tokio::test]
    async fn logs_are_per_key() {
        let e = engine().await;
        e.add_decision(
            "session",
            1,
            DecisionKind::Other,
            "a".into(),
            DecisionAuthor::User,
        )
        .await;
        e.add_decision(
            "swarm:agent-1",
            2,
            DecisionKind::Other,
            "b".into(),
            DecisionAuthor::User,
        )
        .await;
        assert_eq!(e.decisions_for("session").await.entries.len(), 1);
        assert_eq!(e.decisions_for("swarm:agent-1").await.entries.len(), 1);
        assert_eq!(e.decisions_for("swarm:agent-2").await.entries.len(), 0);
    }
