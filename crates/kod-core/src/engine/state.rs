use super::*;

impl KodEngine {
    /// Install an on-disk state store (Tier 3.4). Loads any
    /// previously-saved plans and decision logs into memory. Call
    /// once at engine startup.
    pub async fn set_state_store(&self, store: crate::state::StateStore) {
        // Load whatever is on disk before installing the store, so
        // a caller sees the persisted plans and decisions
        // immediately.
        let loaded = store.load();
        {
            let mut plans = self.plans.write().await;
            for (k, v) in loaded.plans {
                plans.insert(k, v);
            }
        }
        {
            let mut logs = self.decision_logs.write().await;
            for (k, v) in loaded.decision_logs {
                logs.insert(k, v);
            }
        }
        if let Ok(mut slot) = self.state_store.write() {
            *slot = Some(store);
        }
    }

    /// Persist the current plans and decision logs. Best-effort: a
    /// write failure logs and the in-memory state is unchanged.
    pub async fn persist_state(&self) {
        let store = match self.state_store.read() {
            Ok(g) => match g.as_ref() {
                Some(s) => s.clone(),
                None => return,
            },
            Err(_) => return,
        };
        let plans = self.plans.read().await.clone();
        let logs = self.decision_logs.read().await.clone();
        let state = crate::state::EngineState {
            schema_version: crate::state::STATE_SCHEMA_VERSION,
            plans,
            decision_logs: logs,
        };
        if let Err(e) = store.save(&state) {
            tracing::warn!(
                error = %e,
                path = %store.path().display(),
                "could not persist engine state",
            );
        }
    }

    /// Redact secrets from a message list before the prompt is built
    /// (Tier 1.3). No-op when `[security.redact] in_prompt = false`.
    ///
    /// Returns the redacted list and the total number of redactions
    /// that fired. Content is redacted in place; `tool_call_id`,
    /// `role`, and `id` are untouched so the transcript stays
    /// coherent.
    pub fn redact_messages_for_prompt(&self, messages: &mut [kod_types::ChatMessage]) -> usize {
        let cfg = match kod_config::KodConfig::load_cached() {
            Ok(c) => c,
            Err(_) => return 0,
        };
        if !cfg.security.redact.in_prompt {
            return 0;
        }
        let redactor = self.redactor.clone();
        let mut total = 0_usize;
        for m in messages.iter_mut() {
            let (redacted, events) = redactor.redact(&m.content);
            if !events.is_empty() {
                m.content = redacted;
                total += events.iter().map(|e| e.count).sum::<usize>();
            }
        }
        total
    }

    /// Redact secrets from a single tool result's rendered payload.
    /// Called by `cap_rendered_result`'s caller path when in-prompt
    /// redaction is on.
    pub fn redact_tool_result_for_prompt(&self, rendered: String) -> String {
        let cfg = match kod_config::KodConfig::load_cached() {
            Ok(c) => c,
            Err(_) => return rendered,
        };
        if !cfg.security.redact.in_prompt {
            return rendered;
        }
        let (out, _events) = self.redactor.redact(&rendered);
        out
    }

    /// The engine's redactor, but only when the config has opted in
    /// to prompt-path redaction (Tier 1.3). Returns `None` otherwise,
    /// which callers thread into `cap_rendered_result` to skip the
    /// pass.
    pub(crate) fn prompt_redactor_if_enabled(&self) -> Option<&kod_types::redact::Redactor> {
        let cfg = kod_config::KodConfig::load_cached().ok()?;
        if cfg.security.redact.in_prompt {
            Some(&self.redactor)
        } else {
            None
        }
    }

    pub fn cost_tracker(&self) -> &crate::cost::CostTracker {
        &self.cost_tracker
    }

    /// Install the `[limits]` block on the cost tracker (Tier 1.2)
    /// and the per-tool quotas (Tier 2.5).
    pub fn install_limits(&self, cfg: &kod_config::LimitsConfig) {
        self.cost_tracker.install_config(cfg);
        if let Ok(mut g) = self.tool_quotas.write() {
            *g = Some(cfg.tools.clone());
        }
    }

    pub fn set_read_protection(&self, rp: kod_config::ReadProtection) {
        if let Ok(mut slot) = self.read_protection.write() {
            *slot = Some(rp);
        }
    }

    /// Replace the default redactor (Tier 1.3).
    /// Delta §14.1: load or create the vault at the conventional
    /// per-install path, scan the process environment for
    /// secret-shaped variables, register everything found, and
    /// install the result on this engine.
    ///
    /// The key file lives at `~/.kod/secret-placeholder.key` (mode
    /// `0600` on Unix; the store refuses to load a truncated one).
    /// A missing `HOME` is not an error: the engine simply runs
    /// without a vault, which is the pre-§14.1 behavior.
    ///
    /// This is the one call the CLI and TUI make. It is idempotent;
    /// a caller that has already installed a vault replaces it.
    pub async fn install_default_secret_vault(&self) -> std::io::Result<()> {
        let Some(home) = dirs::home_dir() else {
            tracing::debug!("no home directory; secret placeholders are disabled",);
            return Ok(());
        };
        let path = home.join(".kod").join("secret-placeholder.key");
        let vault = match kod_types::secret_placeholder::SecretVault::load_or_create(&path) {
            Ok(v) => std::sync::Arc::new(v),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    path = %path.display(),
                    "could not load secret-placeholder vault; continuing without",
                );
                return Ok(());
            }
        };
        // Scan the environment once at install. The vault is
        // per-process, so a variable that appears later in the
        // process's lifetime is not picked up — an acceptable
        // trade for not touching `std::env` on every turn.
        let discovered = kod_types::secret_sources::scan_env(|| std::env::vars().collect());
        let count = kod_types::secret_sources::register_discovered(&vault, discovered);
        tracing::debug!(
            count,
            path = %path.display(),
            "secret-placeholder vault installed",
        );
        self.set_secret_vault(vault).await;
        Ok(())
    }

    /// Delta §14.1: install a secret-placeholder vault. After this,
    /// every outgoing prompt has registered secrets replaced with
    /// placeholders, and every incoming tool argument has
    /// placeholders replaced with raw values.
    ///
    /// Idempotent. Passing a vault when one is already installed
    /// replaces it — the previous vault's registered secrets become
    /// unknown, which means any placeholder still in flight will
    /// pass through the deobfuscator unchanged. That is the correct
    /// behavior for a test that swaps vaults; a production caller
    /// installs once.
    pub async fn set_secret_vault(
        &self,
        vault: std::sync::Arc<kod_types::secret_placeholder::SecretVault>,
    ) {
        *self.secret_vault.write().await = Some(vault);
    }

    /// Delta §14.1: the current vault, if one is installed.
    pub async fn secret_vault(
        &self,
    ) -> Option<std::sync::Arc<kod_types::secret_placeholder::SecretVault>> {
        self.secret_vault.read().await.clone()
    }

    /// Delta §4.4: install a provider-native compaction block for a
    /// transcript. The `NativeSummary` arm of
    /// `apply_compaction_plan` calls the same storage; this setter
    /// exists so a caller (or a test) can seed one without running
    /// the whole compaction pipeline.
    ///
    /// An empty `encrypted` value is treated as "remove any stored
    /// block" — a cleared compaction should not leave a stale token
    /// behind.
    pub async fn set_native_compaction_block(&self, key: &str, encrypted: &str) {
        let mut blocks = self.native_compaction_blocks.write().await;
        if encrypted.is_empty() {
            blocks.remove(key);
        } else {
            blocks.insert(key.to_string(), encrypted.to_string());
        }
    }

    /// Delta §11.2: the `SessionInit` a previous run wrote for a
    /// session log, if the log carries one.
    ///
    /// A cold revive reads this to rebuild a subagent's tool surface
    /// after a restart. Returns `(endpoint, model, tool_names,
    /// system_prompt_hash)`; the caller compares the fingerprint to
    /// its own rebuilt surface and refuses the revive when they
    /// differ.
    pub fn session_init_from_log(
        log_path: &std::path::Path,
        holder: &str,
    ) -> kod_error::Result<Option<(String, String, Vec<String>, u64)>> {
        crate::session_log::session_init_for(log_path, holder)
    }

    /// Delta §10: enable or disable speculative reads. On by default.
    pub async fn set_speculative_reads(&self, on: bool) {
        *self.speculative_reads.write().await = on;
    }

    /// Delta §4.4: the stored provider-native compaction block for a
    /// transcript, if any.
    pub async fn native_compaction_block(&self, key: &str) -> Option<String> {
        self.native_compaction_blocks.read().await.get(key).cloned()
    }

    /// Delta §14.3: install the TTSR rules. Replaces any existing set.
    /// A rule's regex is compiled here; a bad pattern drops that rule.
    pub async fn set_ttsr_rules(&self, rules: Vec<kod_provider::ttsr::Rule>) {
        *self.ttsr.write().await = kod_provider::ttsr::TtsrEngine::new(rules);
    }

    /// Delta §14.3: install the shipped rules (no-TODO-in-diff,
    /// no-secret-in-prose).
    pub async fn install_builtin_ttsr_rules(&self) {
        self.set_ttsr_rules(kod_provider::ttsr::builtin_rules())
            .await;
    }

    /// Delta §14.3: advance the TTSR engine's turn counter, so a
    /// `Gap(n)` rule can refire.
    pub async fn begin_ttsr_turn(&self) {
        self.ttsr.write().await.begin_turn();
    }

    /// Delta §12.7: seed a mental model. Create-only — a second seed
    /// with the same id is refused, so reloading config mid-session
    /// cannot change a model's definition and move the frozen bytes.
    pub async fn seed_mental_model(
        &self,
        seed: kod_memory::mental_models::MentalModelSeed,
    ) -> bool {
        self.mental_models.write().await.seed(seed)
    }

    /// Delta §12.7: install a model's rendered block. Call only at a
    /// transcript boundary — the block is part of the cacheable prefix.
    pub async fn fill_mental_model(&self, id: &str, text: String) {
        if let Some(m) = self.mental_models.write().await.get_mut(id) {
            m.fill(text);
        }
    }

    /// Delta §12.7: the concatenated model block, for a readout.
    pub async fn mental_models_block(&self) -> String {
        self.mental_models.read().await.render_block()
    }

    /// Delta §12.7: cross a transcript boundary, so a caller can
    /// re-fill models for a new session. The previous blocks are kept
    /// until re-filled (a stale-but-stable summary beats an empty one).
    pub async fn begin_transcript(&self) {
        self.mental_models.write().await.begin_transcript();
    }

    /// Delta §12.7: install the default mental-model seeds and fill
    /// each from the memory store. Called once at `engine.start()`;
    /// a session that starts with no memory is left untouched.
    ///
    /// Seeding is create-only, so a second call (from a test or a
    /// future re-init path) cannot redefine a model and move the
    /// frozen bytes. Filling runs from the store's current contents;
    /// a model whose query returns nothing stays unrendered, so the
    /// default session's prompt is byte-identical to the pre-§12.7
    /// shape.
    ///
    /// The three seeds the design names, with the token budgets it
    /// specifies (600 for preferences, 800 for decisions):
    ///
    /// * `user_preferences` — how the user wants work done.
    /// * `project_conventions` — this repo's rules.
    /// * `project_decisions` — durable choices (the sharpshooter
    ///   pipeline separately feeds `architecture.md`, but the mental
    ///   model is a rolling view the model sees every turn).
    ///
    /// Best-effort: every failure logs and returns; start() never
    /// fails because the seeds could not be filled.
    pub async fn bootstrap_mental_models(&self) {
        use kod_memory::mental_models::{MentalModelSeed, RefreshTrigger};
        if !self.router.has_memory() {
            return;
        }
        let seeds = [
            MentalModelSeed {
                id: "user_preferences".to_string(),
                name: "User Preferences".to_string(),
                source_query: "user preferences coding style conventions".to_string(),
                scopes: Vec::new(),
                max_tokens: 600,
                trigger: RefreshTrigger::SessionStart,
            },
            MentalModelSeed {
                id: "project_conventions".to_string(),
                name: "Project Conventions".to_string(),
                source_query: "project conventions".to_string(),
                scopes: Vec::new(),
                max_tokens: 800,
                trigger: RefreshTrigger::AfterConsolidation,
            },
            MentalModelSeed {
                id: "project_decisions".to_string(),
                name: "Project Decisions".to_string(),
                source_query: "durable project decision chosen architecture".to_string(),
                scopes: Vec::new(),
                max_tokens: 800,
                trigger: RefreshTrigger::AfterConsolidation,
            },
        ];
        for seed in seeds {
            let _ = self.seed_mental_model(seed).await;
        }
        // Bootstrap fills *every* seeded model, regardless of trigger.
        self.fill_mental_models(None).await;
    }

    /// Delta §12.7: fill the mental models whose trigger matches the
    /// caller's context.
    ///
    /// `only_trigger` — `None` fills every model (the bootstrap call);
    /// `Some(t)` fills only the models whose `seed.trigger == t`. The
    /// `AfterConsolidation` refresh uses `Some(AfterConsolidation)`,
    /// leaving the `SessionStart` models frozen.
    ///
    /// Best-effort: a store read that fails skips that model; a model
    /// whose query returns nothing stays unrendered (an empty block is
    /// not injected into the prompt). The lock is held only for the
    /// read of the seed, not across the store read or the fill.
    pub(crate) async fn fill_mental_models(
        &self,
        only_trigger: Option<kod_memory::mental_models::RefreshTrigger>,
    ) {
        let ids = self.mental_models.read().await.ids();
        for id in ids {
            // Read the model's query, cap, and trigger outside the
            // fill lock.
            let (query, cap, trigger) = {
                let guard = self.mental_models.read().await;
                let Some(m) = guard.get(&id) else { continue };
                (
                    m.seed.source_query.clone(),
                    m.seed.max_tokens,
                    m.seed.trigger,
                )
            };
            if let Some(want) = only_trigger
                && trigger != want
            {
                continue;
            }
            let entries = self.router.search_long_term(&query, 40).await;
            if entries.is_empty() {
                continue;
            }
            let text = render_mental_model_block(&entries, cap);
            if text.trim().is_empty() {
                continue;
            }
            self.fill_mental_model(&id, text).await;
            tracing::debug!(id = %id, "mental model filled");
        }
    }

    /// Delta §12.7: re-fill the models whose seed's trigger is
    /// `AfterConsolidation`. The design's rule: a memory consolidation
    /// pass changes what a decision/convention model would contain, so
    /// reload at the *next* transcript boundary (never mid-turn — the
    /// block sits in the cacheable prefix and a mid-turn rewrite
    /// invalidates the provider's prefix cache).
    ///
    /// The periodic memory-consolidation task lives on a router clone
    /// and cannot reach the engine; a caller that owns both — the CLI
    /// or TUI session loop — invokes this method after running
    /// `router.consolidate_memory()`. A future change can wire the
    /// periodic task to call it via a channel; the primitive is what
    /// this lands.
    pub async fn refresh_mental_models_after_consolidation(&self) {
        if !self.router.has_memory() {
            return;
        }
        self.fill_mental_models(Some(
            kod_memory::mental_models::RefreshTrigger::AfterConsolidation,
        ))
        .await;
    }

    /// Delta §14.5: install the OTLP telemetry handle. A caller
    /// that wants export calls this with an enabled handle built
    /// from `Telemetry::from_env`; the default (installed by
    /// `new()`) is disabled and pays nothing.
    pub async fn set_telemetry(&self, telemetry: kod_telemetry::Telemetry) {
        *self.telemetry.write().await = telemetry;
    }

    /// Delta §14.5: a clone of the current telemetry handle. Cloning
    /// is an `Arc` bump; a caller that wants to emit an out-of-band
    /// record uses this.
    pub async fn telemetry(&self) -> kod_telemetry::Telemetry {
        self.telemetry.read().await.clone()
    }

    /// Delta §13.2: the session's behavioral-signal totals.
    pub fn behavioral_signals(&self) -> kod_stats::behavioral::BehavioralSignals {
        *self.behavioral.lock()
    }

    /// Delta §13.2: the per-request aggregates.
    pub fn request_aggregates(&self) -> kod_stats::request::Aggregates {
        self.stats.lock().clone()
    }

    /// Delta §9.11: the run collector's report, for `/stats`.
    pub fn run_report(&self) -> String {
        self.run_collector.lock().report()
    }

    /// Delta §9.11: the run collector, for a caller that wants the
    /// structured numbers rather than the rendered report.
    pub fn run_collector(
        &self,
    ) -> std::sync::Arc<parking_lot::Mutex<crate::run_collector::RunCollector>> {
        std::sync::Arc::clone(&self.run_collector)
    }

    /// Delta §11.6: the current goal, if any. A UI readout.
    pub async fn current_goal(&self) -> Option<crate::goals::Goal> {
        self.goal_runtime.read().await.current().cloned()
    }

    /// Delta §11.6: pause the active goal.
    pub async fn pause_goal(&self) {
        self.goal_runtime.write().await.pause();
    }

    /// Delta §11.6: resume a paused goal.
    pub async fn resume_goal(&self) {
        self.goal_runtime.write().await.resume();
    }

    /// Delta §11.6: drop the active goal.
    pub async fn drop_goal(&self) {
        self.goal_runtime.write().await.drop_current();
    }

    /// Delta §11.6: set a token budget on the active goal. Returns
    /// `false` when there is no goal to attach it to.
    pub async fn set_goal_token_budget(&self, tokens: u64) -> bool {
        let mut rt = self.goal_runtime.write().await;
        match rt.current_mut() {
            Some(g) => {
                g.token_budget = Some(tokens);
                true
            }
            None => false,
        }
    }

    /// Delta §14.1: replace every registered secret in `text` with
    /// its placeholder. No-op when no vault is installed.
    pub async fn obfuscate_secrets(&self, text: &str) -> String {
        match self.secret_vault.read().await.as_ref() {
            Some(v) => v.obfuscate(text),
            None => text.to_string(),
        }
    }

    /// Delta §14.1: replace every placeholder in `text` with the raw
    /// secret. Returns `(restored_text, count)`; count is zero when
    /// no vault is installed.
    pub async fn deobfuscate_secrets(&self, text: &str) -> (String, usize) {
        match self.secret_vault.read().await.as_ref() {
            Some(v) => v.deobfuscate(text),
            None => (text.to_string(), 0),
        }
    }

    /// Delta §14.1: walk a JSON value recursively and deobfuscate
    /// every string. The tool-argument path uses this — arguments
    /// are arbitrary JSON, and the model may have put a placeholder
    /// in a nested field.
    pub(crate) async fn deobfuscate_json(&self, value: &mut serde_json::Value) -> usize {
        let vault = match self.secret_vault.read().await.as_ref() {
            Some(v) => std::sync::Arc::clone(v),
            None => return 0,
        };
        fn walk(
            v: &mut serde_json::Value,
            vault: &kod_types::secret_placeholder::SecretVault,
        ) -> usize {
            let mut n = 0;
            match v {
                serde_json::Value::String(s) => {
                    let (out, c) = vault.deobfuscate(s);
                    if c > 0 {
                        *s = out;
                        n += c;
                    }
                }
                serde_json::Value::Array(a) => {
                    for item in a.iter_mut() {
                        n += walk(item, vault);
                    }
                }
                serde_json::Value::Object(o) => {
                    for (_, item) in o.iter_mut() {
                        n += walk(item, vault);
                    }
                }
                _ => {}
            }
            n
        }
        walk(value, &vault)
    }

    pub fn set_redactor(&mut self, redactor: kod_types::redact::Redactor) {
        self.redactor = std::sync::Arc::new(redactor);
    }

    /// The current read-protection, if any.
    pub fn read_protection_setting(&self) -> Option<kod_config::ReadProtection> {
        self.read_protection.read().ok().and_then(|g| g.clone())
    }

    /// The current redactor (shared handle).
    pub fn redactor(&self) -> std::sync::Arc<kod_types::redact::Redactor> {
        self.redactor.clone()
    }

    pub fn set_sandbox_mode(&self, mode: kod_tools::context::SandboxMode) {
        use std::sync::atomic::Ordering;
        let v = match mode {
            kod_tools::context::SandboxMode::Disabled => 0u8,
            kod_tools::context::SandboxMode::Auto => 2u8,
            kod_tools::context::SandboxMode::Require => 1u8,
        };
        self.sandbox_mode_atomic.store(v, Ordering::Relaxed);
    }

    /// The effective sandbox state as a caller (the TUI header, a
    /// status panel) wants to render it: the configured mode plus the
    /// backend that will actually be used.
    ///
    /// The second element is the resolver's chosen primitive name
    /// (`"bwrap"`, `"landlock"`, `"sandbox-exec"`) when one is
    /// available, or `None` when the caller has Auto mode and no
    /// primitive is installed — the honest "off" case the design's
    /// AD-10 wants visible.
    pub fn sandbox_status(&self) -> (kod_tools::context::SandboxMode, Option<&'static str>) {
        let mode = self.sandbox_setting();
        // `Disabled` never queries the resolver; the caller asked for
        // no sandbox and that is what they get.
        if matches!(mode, kod_tools::context::SandboxMode::Disabled) {
            return (mode, None);
        }
        let resolver = kod_tools::context::SandboxResolver::detect();
        (mode, resolver.backend_name())
    }

    /// The current sandbox mode.
    pub fn sandbox_setting(&self) -> kod_tools::context::SandboxMode {
        use std::sync::atomic::Ordering;
        match self.sandbox_mode_atomic.load(Ordering::Relaxed) {
            1 => kod_tools::context::SandboxMode::Require,
            2 => kod_tools::context::SandboxMode::Auto,
            _ => kod_tools::context::SandboxMode::Disabled,
        }
    }

    /// Enable or disable network access for `web_fetch`. The CLI and
    /// TUI call this at startup with `LlmConfig::network_access`. A
    /// caller that never calls it gets the default (off) — a session
    /// that has not opted in cannot reach the network through a tool.
    pub fn set_network_access(&self, allowed: bool) {
        self.network_access_atomic
            .store(allowed, std::sync::atomic::Ordering::Relaxed);
    }

    /// The current network-access setting.
    pub fn network_access_setting(&self) -> bool {
        self.network_access_atomic
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Enable or disable auto-check after writes. Called by the CLI and
    /// TUI at startup with `ToolsConfig::auto_check`.
    pub fn set_auto_check(&self, enabled: bool) {
        self.auto_check_atomic
            .store(enabled, std::sync::atomic::Ordering::Relaxed);
    }

    /// The current auto-check setting.
    pub fn auto_check_setting(&self) -> bool {
        self.auto_check_atomic
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Enable or disable the post-write LSP diagnostics pass. Called
    /// by the CLI/TUI at startup with `ToolsConfig::auto_lsp`.
    pub fn set_auto_lsp(&self, enabled: bool) {
        self.auto_lsp_atomic
            .store(enabled, std::sync::atomic::Ordering::Relaxed);
    }

    /// The current auto-LSP setting.
    pub fn auto_lsp_setting(&self) -> bool {
        self.auto_lsp_atomic
            .load(std::sync::atomic::Ordering::Relaxed)
    }
}
