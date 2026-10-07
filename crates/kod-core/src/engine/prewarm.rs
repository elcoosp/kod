//! Warm-path state: prewarm / tab-bridge bookkeeping.
//!
//! Extracted from `engine/mod.rs`.

use super::*;

impl KodEngine {
    /// WS-C: whether the default endpoint is a tab bridge. The TUI
    /// and [`Self::prewarm`] consult this for the prewarm policy, and
    /// WS-A consults it for background-session stamping. Approximation:
    /// prewarm and background traffic both resolve through the default
    /// chain, so the default endpoint is the honest "active endpoint".
    pub(crate) async fn tab_bridge_active(&self) -> bool {
        match kod_config::KodConfig::load_cached() {
            Ok(c) => c.llm.default_endpoint().tab_bridge,
            Err(_) => false,
        }
    }

    /// WS-C: whether the keystroke prewarm probe may run. `auto`
    /// (default) disables it on tab-bridge endpoints, where the probe
    /// would warm the wrong tab.
    pub async fn prewarm_enabled(&self) -> bool {
        let (mode, tab_bridge) = match kod_config::KodConfig::load_cached() {
            Ok(c) => (c.llm.prewarm, c.llm.default_endpoint().tab_bridge),
            // An unreadable config must not change runtime behavior.
            Err(_) => return true,
        };
        match mode {
            kod_config::llm::PrewarmMode::On => true,
            kod_config::llm::PrewarmMode::Off => false,
            kod_config::llm::PrewarmMode::Auto => !tab_bridge,
        }
    }

    pub async fn prewarm(&self, key: &str) {
        // WS-C: policy gate first (defense in depth — the TUI checks
        // `prewarm_enabled` before spawning, but direct callers land
        // here too).
        if !self.prewarm_enabled().await {
            return;
        }
        // Latch first, so a burst of keystrokes does not queue a burst
        // of requests behind the first one.
        {
            let mut g = self.prewarmed.write().await;
            if !g.insert(key.to_string()) {
                return;
            }
        }

        let Some(provider) = self.current_provider().await else {
            return;
        };
        // No trace means no prompt has been built yet — a fresh
        // session has nothing cached to warm.
        let Some(trace) = self.last_prompt_trace_for(key).await else {
            return;
        };

        // Only the cacheable head is worth sending: the volatile tail
        // (environment, tool inventory, memory) differs every turn and
        // would not be a cache hit anyway.
        const VOLATILE_MARKER: &str = "## Volatile suffix";
        let cacheable = match trace.text.find(VOLATILE_MARKER) {
            Some(i) => trace.text[..i].trim_end(),
            None => return,
        };
        if cacheable.is_empty() {
            return;
        }

        let mut system = kod_provider::request::SystemPrompt::new();
        system = system.with(cacheable.to_string(), true);

        let model = self.current_model.read().await.clone();
        let req = kod_provider::request::CompletionRequest {
            system,
            messages: vec![kod_types::ChatMessage::text(
                kod_types::MessageId::new(),
                kod_types::MessageRole::User,
                "warm",
                time::OffsetDateTime::now_utc(),
            )],
            tools: Vec::new(),
            // One output token: the response is discarded, and a
            // larger budget is money spent for nothing.
            options: kod_provider::GenerationOptions {
                model: None,
                max_tokens: Some(1),
                temperature: Some(0.0),
                top_p: None,
                stop_sequences: Vec::new(),
                // A prewarm is not a reasoning call; it should fail
                // fast rather than wait out a thinking timeout.
                effort: Some(kod_provider::effort::EffortLevel::None),
                tool_choice: None,
            },
            model,
            cache_transcript: false,
            native_compaction_block: None,
            image_frames: Vec::new(),
            // Prewarm stays sessionless: it renders a literal "warm" probe
            // turn, which must never land in the session's tab or chain.
            // (On a tab backend it mints a throwaway anon session on some
            // other tab — pure cost, no benefit — which is why
            // `prewarm = "auto"` disables the probe there. The bridge
            // releases the ephemeral tab after the turn, so when the
            // probe does run it is recycled, not leaked.)
            session_id: None,
        };

        // Bounded: a provider that hangs must not leave a task
        // parked forever. Five seconds is longer than a warm cache
        // read and shorter than a user's typing. Note the timeout only
        // abandons kod's wait: on a bridge backend the turn keeps
        // running server-side and holds its tab to completion.
        let _ =
            tokio::time::timeout(std::time::Duration::from_secs(5), provider.complete(&req)).await;
    }

    /// Clear the prewarm latch so the next keystroke of a new turn
    /// warms again.
    pub(crate) async fn reset_prewarm(&self, key: &str) {
        self.prewarmed.write().await.remove(key);
    }
}
