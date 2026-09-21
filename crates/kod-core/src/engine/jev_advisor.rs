//! Jev advisor helpers for [`super::KodEngine`].
//!
//! The `*_with_jev` family used to live inline in `engine/mod.rs`.
//! Moving each one here keeps the entire Jev dependency surface —
//! the network client, the decision log, the request lookup — in one
//! file. As a submodule of `engine`, this file has access to
//! `KodEngine`'s private fields and private helper methods; no
//! visibility changes are required on the parent.

use super::KodEngine;

impl KodEngine {
    pub async fn classify_chunk_with_jev(&self, holder: &str, buffer_tail: &str) -> Option<String> {
        let jev = self.jev_client()?;
        let request = self.current_request(holder).await.unwrap_or_default();
        let state = crate::jev::build_state(
            &format!(
                "User request: {request}\n\nRecent buffer: {}",
                crate::jev::preview_chars(buffer_tail, 500),
            ),
            &[],
        );
        let labels = &["prose_answer", "reasoning", "restatement", "code_block"];
        let started = std::time::Instant::now();
        let decision = jev
            .evaluate_score(&state, "What kind of text is this streamed chunk?", labels)
            .await
            .ok()?;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        self.log_jev_decision(
            holder,
            "chunk_classify",
            &crate::jev::preview_chars(buffer_tail, 200),
            "chunk_kind",
            serde_json::json!({ "kind": decision.value }),
            decision.confidence,
            elapsed_ms,
            false,
            crate::jev::DecisionSource::Jev,
        );
        Some(decision.value)
    }
}
