//! The context gauge must be cleared when a compaction mutates the
//! transcript's prefix.
//!
//! Delta §2.4: the gauge anchors on the provider's own reported
//! `prompt_tokens` at a specific index in the transcript. Every
//! compaction path that removes or truncates messages below that
//! index must clear the anchor — otherwise `anchored_context_tokens`
//! returns `anchor + tail`, where `anchor` is the pre-compaction
//! prompt-token count. The reported window is then overstated by the
//! dropped prefix, the compaction trigger fires late, and the next
//! provider request goes out larger than the caller believes.
//!
//! `compact_history_for` (transcript.rs) already cleared the gauge
//! for the same reason. This test pins the invariant on the two paths
//! that did not: `apply_compaction_plan` (the mechanical + summary
//! path) and the emergency fallback in `maybe_compact_for`.

use kod_core::KodEngine;
use kod_core::router::RouterConfig;
use kod_types::{ChatMessage, MessageId, MessageRole};
use std::sync::Arc;
use tempfile::TempDir;
use time::OffsetDateTime;

fn config(dir: &std::path::Path) -> RouterConfig {
    RouterConfig {
        context_window: 8192,
        short_term_capacity: 100,
        working_dir: dir.to_path_buf(),
        enable_memory: false,
        ..RouterConfig::default()
    }
}

fn msg(role: MessageRole, content: &str) -> ChatMessage {
    ChatMessage::text(MessageId::new(), role, content, OffsetDateTime::now_utc())
}

async fn seeded_engine(n_turns: usize) -> (Arc<KodEngine>, TempDir) {
    let tmp = TempDir::new().unwrap();
    let engine = Arc::new(KodEngine::new(config(tmp.path()), tmp.path().join("t.kod")).unwrap());
    let mut turns = Vec::with_capacity(n_turns);
    for i in 0..n_turns {
        turns.push(msg(
            if i % 2 == 0 {
                MessageRole::User
            } else {
                MessageRole::Assistant
            },
            &format!("turn {i} content {}", "x".repeat(500)),
        ));
    }
    engine.set_history_for_tests("", turns).await;
    (engine, tmp)
}

/// After a compaction, the gauge must not be readable (cleared). The
/// pre-fix shape left the anchor in place and
/// `anchored_context_tokens` reported the stale pre-compaction number.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_compaction_clears_the_context_gauge() {
    let (engine, _tmp) = seeded_engine(20).await;

    // Anchor the gauge as if the provider had just reported 60 000
    // prompt tokens over the current transcript.
    engine.gauge_observe_for_tests("", 0, 60_000).await;
    let before = engine.anchored_context_tokens_for_tests("").await;
    assert!(
        before.is_some(),
        "the gauge must be anchored before the test"
    );

    // Compact the transcript — the drop removes the prefix the
    // anchor describes.
    let affected = engine.compact_history_for_tests("", 4).await;
    assert!(affected > 0, "compaction must have dropped something");

    // Post-fix: the gauge is gone.
    let after = engine.anchored_context_tokens_for_tests("").await;
    assert!(
        after.is_none(),
        "compaction must clear the anchor; got {after:?}",
    );
}

/// A manual compaction must summarise what it drops, not discard it.
///
/// `/compact` used to `drain` the front of the transcript and leave
/// nothing behind, so every fact, path and decision from those turns
/// vanished from the model's view -- while the automatic threshold path
/// summarised the same kind of prefix. This pins the manual path to the
/// same shape.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_manual_compaction_leaves_a_summary_behind() {
    let (engine, _tmp) = seeded_engine(20).await;

    let dropped = engine.compact_history_for_tests("", 4).await;
    assert_eq!(dropped, 16, "16 of 20 turns are dropped");

    let turns = engine.history_for("").await;
    // 4 retained + 1 summary.
    assert_eq!(turns.len(), 5, "retained suffix plus the summary message");

    let summary = &turns[0];
    assert_eq!(
        summary.role,
        kod_types::MessageRole::User,
        "the summary is a user-role block the model reads as context",
    );
    assert!(
        summary
            .content
            .starts_with("## Previous Conversation Summary"),
        "the summary must be identifiable",
    );
    assert!(
        summary.metadata.pinned,
        "the summary is pinned so the render path cannot drop it",
    );
}

/// A pinned message must survive an explicit compaction.
///
/// `render_history_for` honours `metadata.pinned` (the budget trim
/// walks its cut back to the earliest pinned message), and the docs
/// promise `/pin <n>` "protects a message from being compacted away".
/// `compact_history_for` computed a bare suffix cut and dropped the
/// pinned message anyway.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pinned_message_survives_a_manual_compaction() {
    let (engine, _tmp) = seeded_engine(20).await;

    // Pin the first turn.
    let mut turns = engine.history_for("").await;
    turns[0].metadata.pinned = true;
    engine.set_history_for_tests("", turns).await;

    engine.compact_history_for_tests("", 4).await;

    let after = engine.history_for("").await;
    assert!(
        after.iter().any(|m| m.content.contains("turn 0")),
        "a pinned message must survive; got {:?}",
        after.iter().map(|m| m.content.clone()).collect::<Vec<_>>(),
    );
}

/// When every message is pinned, compaction is a no-op rather than a
/// silent unpin.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_all_pinned_transcript_is_not_compacted() {
    let (engine, _tmp) = seeded_engine(8).await;

    let mut turns = engine.history_for("").await;
    for t in turns.iter_mut() {
        t.metadata.pinned = true;
    }
    engine.set_history_for_tests("", turns).await;

    engine.compact_history_for_tests("", 2).await;
    // The helper reports drops from the *requested* keep-count, which
    // is not the count actually applied once pinned messages are
    // honoured. The transcript is the invariant that matters.
    assert_eq!(
        engine.history_for("").await.len(),
        8,
        "nothing may be dropped when every message is pinned",
    );
}
