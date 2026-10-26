use super::*;

/// Blank the `arguments` of every tool call whose path argument
/// matches one of `patterns` (from
/// `[security.redact] redact_argument_paths`).
///
/// Pure so it is unit-testable without a `KodEngine` or a config file.
/// Returns the number of calls whose arguments were replaced.
///
/// The doc comment on `RedactConfig::redact_argument_paths` promises
/// "the model cannot see the arguments at all", so this is a
/// wholesale replacement: the call keeps its tool name and id, but
/// its `arguments` object becomes a single explanatory field. Keeping
/// a well-formed JSON object (rather than a bare string or `null`)
/// means the provider wire layer and any downstream parser see a
/// valid `ToolCall`.
pub(crate) fn redact_tool_call_arguments(
    patterns: &[String],
    messages: &mut [kod_types::ChatMessage],
    working_dir: &std::path::Path,
) -> usize {
    if patterns.is_empty() {
        return 0;
    }
    let mut n = 0_usize;
    for m in messages.iter_mut() {
        for call in m.tool_calls.iter_mut() {
            if kod_config::policy::arguments_path_matches_redact_list(
                patterns,
                &call.arguments,
                working_dir,
            ) {
                call.arguments = serde_json::json!({
                    "redacted": "arguments matched security.redact.redact_argument_paths",
                });
                n += 1;
            }
        }
    }
    n
}

/// Redact every message's `content` through `redactor`, memoised.
///
/// Returns the number of redactions that fired. A message whose
/// content equals either the input it was last scanned from or the
/// output that scan produced is served from `memo` instead of being
/// re-scanned. The second form is the one that matters in practice:
/// the caller assigns the redacted text back into `content`, so from
/// the second round onward the stored input no longer matches but the
/// stored output does.
///
/// **Why content equality and not a digest.** A memo hit skips the
/// scan, so a false hit would leave a message's secret in place. A
/// hash collision would be exactly that; string equality cannot
/// collide. The stored strings cost one message's worth of bytes per
/// entry, which is bounded by the transcript.
///
/// **Why the memo is keyed on id as well as content.** Two messages
/// can hold identical text -- a repeated `read_file` result, say --
/// and each must be redacted on its own first sighting, so the entry
/// has to be per message, not per unique string.
///
/// `events`-less scans (content with no secret) still populate the
/// entry: the next round finds the same clean text and skips the
/// scan, which is the common case on a large transcript.
pub(crate) fn redact_contents_memoised(
    redactor: &kod_types::redact::Redactor,
    messages: &mut [kod_types::ChatMessage],
    memo: &mut std::collections::HashMap<kod_types::MessageId, (String, String)>,
) -> usize {
    let mut total = 0_usize;
    for m in messages.iter_mut() {
        if let Some((input, output)) = memo.get(&m.id)
            && (m.content == *input || m.content == *output)
        {
            m.content = output.clone();
            continue;
        }
        let input = m.content.clone();
        let (redacted, events) = redactor.redact(&input);
        let output = if events.is_empty() {
            input.clone()
        } else {
            redacted
        };
        if !events.is_empty() {
            total += events.iter().map(|e| e.count).sum::<usize>();
        }
        memo.insert(m.id.clone(), (input, output.clone()));
        m.content = output;
    }
    // Drop entries for messages a compaction removed. The guard keeps
    // the common case (nothing dropped) free of the retain pass.
    if memo.len() > messages.len() {
        let live: std::collections::HashSet<&kod_types::MessageId> =
            messages.iter().map(|m| &m.id).collect();
        memo.retain(|id, _| live.contains(id));
    }
    total
}

impl KodEngine {
    /// Install an on-disk state store (Tier 3.4). Loads any
    /// previously-saved plans and decision logs into memory. Call
    /// once at engine startup.
    pub async fn set_state_store(&self, store: kod_core_state::state::StateStore) {
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
        let state = kod_core_state::state::EngineState {
            schema_version: kod_core_state::state::STATE_SCHEMA_VERSION,
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
    /// (Tier 1.3).
    ///
    /// Two independent switches, both under `[security.redact]`:
    ///
    /// * `in_prompt` runs the one-way [`kod_types::redact::Redactor`]
    ///   over every message's `content`.
    /// * `redact_argument_paths` blanks the `arguments` of any tool
    ///   call whose path argument matches one of the globs. This is
    ///   independent of `in_prompt`: a user may want argument
    ///   redaction on while leaving result redaction off (a normal
    ///   coding session needs un-redacted `read_file` results, but a
    ///   `write_file` about to place a literal credential should not
    ///   carry it into the prompt).
    ///
    /// Returns the total number of redactions that fired. Content is
    /// redacted in place; `tool_call_id`, `role`, and `id` are
    /// untouched so the transcript stays coherent.
    pub fn redact_messages_for_prompt(&self, messages: &mut [kod_types::ChatMessage]) -> usize {
        let cfg = match kod_config::KodConfig::load_cached() {
            Ok(c) => c,
            Err(_) => return 0,
        };

        let mut total = 0_usize;

        // (a) Argument-path redaction, independent of `in_prompt`.
        total += redact_tool_call_arguments(
            &cfg.security.redact.redact_argument_paths,
            messages,
            &self.working_dir,
        );

        // (b) Content redaction, gated on `in_prompt`.
        if cfg.security.redact.in_prompt {
            let redactor = self.redactor.clone();
            let mut memo = self.redaction_memo.lock();
            total += redact_contents_memoised(&redactor, messages, &mut memo);
        }

        total
    }

    /// Redact secrets from a rendered tool result when
    /// `[security.redact] in_prompt = true`; return it unchanged
    /// otherwise.
    ///
    /// **No production caller.** The live path is
    /// `render::cap_rendered_result`, which takes `Option<&Redactor>`
    /// and is reached from `tool_dispatch` via
    /// `prompt_redactor_if_enabled()`; the `Error` and
    /// `RequiresConfirmation` arms go through
    /// `render::redact_rendered_text`. The pre-fix doc claimed this
    /// method was called by "`cap_rendered_result`'s caller path",
    /// which was never true.
    ///
    /// Kept rather than deleted: `KodEngine` is re-exported from
    /// `kod-core`'s root, so this is public API and removing it is a
    /// breaking change. It is a thin, correct wrapper -- a caller that
    /// wants the config check and the redaction in one call can use
    /// it. If it should instead be the routing point for the tool
    /// result path, that is a decision about the engine's public
    /// surface, not a drive-by change.
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

    pub fn cost_tracker(&self) -> &kod_core_state::cost::CostTracker {
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
        kod_core_state::session_log::session_init_for(log_path, holder)
    }

    /// Delta §10: enable or disable speculative reads. On by default.
    /// Delta §4.4: the stored provider-native compaction block for a
    /// transcript, if any.
    pub async fn native_compaction_block(&self, key: &str) -> Option<String> {
        self.native_compaction_blocks.read().await.get(key).cloned()
    }

    /// Delta §14.3: install the TTSR rules. Replaces any existing set.
    /// A rule's regex is compiled here; a bad pattern drops that rule.
    pub async fn set_ttsr_rules(&self, rules: Vec<kod_provider::ttsr::Rule>) {
        // PERF-11: record whether any rules are installed so the hot
        // stream loop can short-circuit the lock when the engine is
        // empty (the default state).
        let has = !rules.is_empty();
        *self.ttsr.write().await = kod_provider::ttsr::TtsrEngine::new(rules);
        self.ttsr_has_rules
            .store(has, std::sync::atomic::Ordering::Relaxed);
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

    /// Delta §12.7: drain the "consolidation moved facts" flag the
    /// periodic task sets. When set, re-fill the models whose seed
    /// triggers on `AfterConsolidation` (the decision/convention
    /// models — see [`Self::fill_mental_models`] for what the trigger
    /// filter means). Runs at the *start* of a turn, never mid-turn:
    /// the model block sits in the cacheable prefix and a mid-turn
    /// rewrite would invalidate the provider's prefix cache.
    ///
    /// Idempotent and cheap: a false flag short-circuits before the
    /// memory lookup.
    /// Test-only: force the consolidation-refresh flag, so a test can
    /// assert the drain clears it without waiting out a real
    /// consolidation interval.
    pub fn set_mental_models_due_for_tests(&self, due: bool) {
        self.mental_models_due
            .store(due, std::sync::atomic::Ordering::Relaxed);
    }

    /// Test-only: read the consolidation-refresh flag.
    pub fn mental_models_due_for_tests(&self) -> bool {
        self.mental_models_due
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub(crate) async fn drain_due_mental_models_refresh(&self) {
        if !self
            .mental_models_due
            .swap(false, std::sync::atomic::Ordering::Relaxed)
        {
            return;
        }
        if !self.router.has_memory() {
            return;
        }
        self.fill_mental_models(Some(
            kod_memory::mental_models::RefreshTrigger::AfterConsolidation,
        ))
        .await;
    }

    /// Tier 1.4: allocate the next turn id and return it. Called by
    /// the swarm runner once per `run` to mint a "run id" that every
    /// dispatch's trace records as its parent. The id is drawn from
    /// the same counter as ordinary turn ids, so it is globally
    /// unique; it is *not* emitted as a `TurnTrace` — the run has no
    /// single model round to record.
    pub fn begin_swarm_run(&self) -> kod_core_state::trace::TurnId {
        self.next_turn_id()
    }

    /// Tier 1.4: record the parent for the next trace on `key`.
    /// Idempotent overwrite. `process_streaming_with_model_for` takes
    /// the entry when it opens the trace.
    pub async fn set_pending_trace_parent(&self, key: &str, parent: kod_core_state::trace::TurnId) {
        self.pending_trace_parent
            .write()
            .await
            .insert(key.to_string(), parent);
    }

    /// Tier 1.4: take the pending parent for `key`, if any. Drained
    /// on read, so a second `process_streaming*` on the same key does
    /// not inherit a stale parent.
    pub(crate) async fn take_pending_trace_parent(
        &self,
        key: &str,
    ) -> Option<kod_core_state::trace::TurnId> {
        self.pending_trace_parent.write().await.remove(key)
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
    pub async fn current_goal(&self) -> Option<kod_core_state::goals::Goal> {
        self.goal_runtime.read().await.current().cloned()
    }

    /// Delta §11.6: pause the active goal.
    /// Delta §11.6: resume a paused goal.
    /// Delta §11.6: drop the active goal.
    /// Delta §11.6: set a token budget on the active goal. Returns
    /// `false` when there is no goal to attach it to.
    /// Delta §14.1: replace every registered secret in `text` with
    /// its placeholder. No-op when no vault is installed.
    /// Delta §14.1: replace every placeholder in `text` with the raw
    /// secret. Returns `(restored_text, count)`; count is zero when
    /// no vault is installed.
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

    /// Obfuscate registered secrets in every string of a JSON value.
    ///
    /// Mirror of [`Self::deobfuscate_json`], for the outbound path.
    /// `obfuscate` is applied to message `content` at the request
    /// build site, but a tool call's `arguments` also reach the
    /// provider (as the assistant message's `tool_calls`). Pre-fix
    /// those arguments were never scrubbed, so a registered secret the
    /// model put into a tool call leaked on every subsequent request.
    pub(crate) async fn obfuscate_json(&self, value: &mut serde_json::Value) -> usize {
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
                    let out = vault.obfuscate(s);
                    if out != *s {
                        *s = out;
                        n += 1;
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

    /// A copy of `turns` with every registered secret obfuscated in
    /// message content and tool-call arguments. Returns a clone
    /// unchanged when no vault is installed.
    ///
    /// The compaction rungs (`handoff`, `remote`, `snapcompact`) build
    /// their own provider requests from the transcript, bypassing the
    /// outbound obfuscation in `build_grounded_request`. Without this,
    /// a secret in the transcript reached the summarizer in the clear —
    /// and the summary it produced then carried the raw value forward.
    pub(crate) async fn obfuscated_transcript(
        &self,
        turns: &[kod_types::ChatMessage],
    ) -> Vec<kod_types::ChatMessage> {
        let vault = match self.secret_vault.read().await.as_ref() {
            Some(v) if !v.is_empty() => std::sync::Arc::clone(v),
            _ => return turns.to_vec(),
        };
        turns
            .iter()
            .map(|m| {
                let mut m = m.clone();
                m.content = vault.obfuscate(&m.content);
                for call in m.tool_calls.iter_mut() {
                    fn walk(
                        v: &mut serde_json::Value,
                        vault: &kod_types::secret_placeholder::SecretVault,
                    ) {
                        match v {
                            serde_json::Value::String(s) => *s = vault.obfuscate(s),
                            serde_json::Value::Array(a) => {
                                for item in a.iter_mut() {
                                    walk(item, vault);
                                }
                            }
                            serde_json::Value::Object(o) => {
                                for (_, item) in o.iter_mut() {
                                    walk(item, vault);
                                }
                            }
                            _ => {}
                        }
                    }
                    walk(&mut call.arguments, &vault);
                }
                m
            })
            .collect()
    }

    /// Scan `text` for vendor-shaped secrets and connection-URL
    /// passwords, registering each with the vault. Returns the count
    /// of newly registered values.
    ///
    /// The vault's env scan (`install_default_secret_vault`) only sees
    /// secrets declared in the environment. A vendor-shaped literal —
    /// an `sk-ant-…`, a `ghp_…`, a `postgres://user:pw@host` password —
    /// that appears in tool output was never registered, so the
    /// outbound obfuscation pass had nothing to substitute and it
    /// reached the provider in the clear. `scan_text` is the detector
    /// for exactly that case; this wires it into the tool-result path.
    ///
    /// A no-op when no vault is installed (the whole mechanism is off)
    /// or the scan finds nothing.
    pub(crate) async fn register_secrets_in_text(&self, text: &str) -> usize {
        let vault = match self.secret_vault.read().await.as_ref() {
            Some(v) => std::sync::Arc::clone(v),
            None => return 0,
        };
        let discovered = kod_types::secret_sources::scan_text(text);
        if discovered.is_empty() {
            return 0;
        }
        let n = kod_types::secret_sources::register_discovered(&vault, discovered);
        if n > 0 {
            tracing::debug!(count = n, "registered vendor/URL secrets from tool output");
        }
        n
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
    /// Restore the transcript for `key` from a session log.
    ///
    /// This is the P2 rehydration entry point: it reads the JSONL
    /// written by a prior process (via [`kod_core_state::session_log::read_session`]),
    /// rebuilds the tool calls that ran under `key` as prose turns,
    /// and appends them to the live history. The messages are
    /// User-role summaries so they survive the text-protocol
    /// `render_history` filter that deliberately skips Tool-role
    /// rows; see [`kod_core_state::session_log::rehydrate_prose_turns`] for
    /// the rationale and the structured sibling
    /// [`kod_core_state::session_log::rehydrate_turns`] for the future
    /// `CompletionRequest` path.
    ///
    /// Returns the number of messages appended. A missing or empty
    /// log yields `Ok(0)` and leaves history untouched.
    pub async fn rehydrate_from_log_for(&self, key: &str, path: &std::path::Path) -> Result<usize> {
        self.rehydrate_from_log_with(
            key,
            path,
            kod_core_state::session_log::RehydrationMode::Prose,
        )
        .await
    }

    /// Rehydrate with an explicit mode. `rehydrate_from_log_for` is
    /// the `Prose` convenience.
    pub async fn rehydrate_from_log_with(
        &self,
        key: &str,
        path: &std::path::Path,
        mode: kod_core_state::session_log::RehydrationMode,
    ) -> Result<usize> {
        let entries = kod_core_state::session_log::read_session(path)?;
        let messages = match mode {
            kod_core_state::session_log::RehydrationMode::Prose => {
                kod_core_state::session_log::rehydrate_prose_turns(&entries, key)
            }
            kod_core_state::session_log::RehydrationMode::Structured => {
                kod_core_state::session_log::rehydrate_turns(&entries, key)
            }
        };
        let count = messages.len();
        if count == 0 {
            return Ok(0);
        }
        let mut guard = self.history.write().await;
        guard.entry(key.to_string()).or_default().extend(messages);
        Ok(count)
    }
}

#[cfg(test)]
mod redaction_memo_tests {
    //! The memo must never serve a hit for content it did not scan.
    //!
    //! A false hit skips the redaction pass and leaves a secret in the
    //! message that reaches the model, so every case here is a
    //! correctness test, not a performance one. The pure function takes
    //! the memo as a parameter precisely so this can be driven without
    //! a `KodEngine` or a config file.

    use super::*;
    use kod_types::{ChatMessage, MessageId, MessageRole};

    fn msg(id: &MessageId, content: &str) -> ChatMessage {
        ChatMessage::text(
            id.clone(),
            MessageRole::User,
            content.to_string(),
            time::OffsetDateTime::now_utc(),
        )
    }

    fn memo() -> std::collections::HashMap<MessageId, (String, String)> {
        std::collections::HashMap::new()
    }

    const SECRET: &str = "api_key = sk-abcdef0123456789abcdef0123456789";

    #[test]
    fn first_pass_redacts_and_records() {
        let r = kod_types::redact::Redactor::default();
        let id = MessageId::new();
        let mut msgs = vec![msg(&id, SECRET)];
        let mut m = memo();

        let n = redact_contents_memoised(&r, &mut msgs, &mut m);

        assert_eq!(n, 1, "one redaction fires");
        assert!(!msgs[0].content.contains("sk-abcdef"), "secret removed");
        assert!(msgs[0].content.contains("[REDACTED:openai-key]"));
        assert_eq!(m.len(), 1, "the scan is recorded");
    }

    #[test]
    fn second_pass_on_the_redacted_text_is_a_free_hit() {
        let r = kod_types::redact::Redactor::default();
        let id = MessageId::new();
        let mut msgs = vec![msg(&id, SECRET)];
        let mut m = memo();

        let first = redact_contents_memoised(&r, &mut msgs, &mut m);
        let after_first = msgs[0].content.clone();
        let second = redact_contents_memoised(&r, &mut msgs, &mut m);

        assert_eq!(first, 1);
        assert_eq!(second, 0, "no redaction fires on already-redacted text");
        assert_eq!(msgs[0].content, after_first, "content is unchanged");
    }

    /// The security-critical case. A message whose content is replaced
    /// between rounds must be re-scanned: the memo holds the *old*
    /// content, so serving its output would ship whatever the new
    /// content carries.
    #[test]
    fn mutated_content_is_rescanned_not_served_from_the_memo() {
        let r = kod_types::redact::Redactor::default();
        let id = MessageId::new();
        let mut msgs = vec![msg(&id, "clean text with no secret")];
        let mut m = memo();

        assert_eq!(redact_contents_memoised(&r, &mut msgs, &mut m), 0);

        // The same message now carries a secret.
        msgs[0].content = SECRET.to_string();
        let n = redact_contents_memoised(&r, &mut msgs, &mut m);

        assert_eq!(n, 1, "the new content must be scanned");
        assert!(
            !msgs[0].content.contains("sk-abcdef"),
            "the secret must be redacted, got {:?}",
            msgs[0].content,
        );
    }

    /// A message id reused for different content -- the caller reset
    /// the transcript, or a compaction renumbered -- must not alias.
    #[test]
    fn two_messages_with_the_same_text_each_scan_once() {
        let r = kod_types::redact::Redactor::default();
        let a = MessageId::new();
        let b = MessageId::new();
        let mut msgs = vec![msg(&a, SECRET), msg(&b, SECRET)];
        let mut m = memo();

        let n = redact_contents_memoised(&r, &mut msgs, &mut m);

        assert_eq!(n, 2, "both messages fire independently");
        assert!(!msgs[0].content.contains("sk-abcdef"));
        assert!(!msgs[1].content.contains("sk-abcdef"));
        assert_eq!(m.len(), 2, "one entry per message id");
    }

    #[test]
    fn entries_for_dropped_messages_are_evicted() {
        let r = kod_types::redact::Redactor::default();
        let kept = MessageId::new();
        let dropped = MessageId::new();
        let mut msgs = vec![msg(&kept, SECRET), msg(&dropped, SECRET)];
        let mut m = memo();

        redact_contents_memoised(&r, &mut msgs, &mut m);
        assert_eq!(m.len(), 2);

        // A compaction drops the second message.
        msgs.truncate(1);
        redact_contents_memoised(&r, &mut msgs, &mut m);

        assert_eq!(m.len(), 1, "the dropped id's entry is gone");
        assert!(m.contains_key(&kept));
    }
    /// Not a correctness test: a before/after of the memo on the shape
    /// addendum 23.4 measured (50 messages x 2 KB, 40 rounds).
    /// `#[ignore]` so CI never pays for it; run with
    /// `cargo test -p kod-core memo_timing -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn memo_timing() {
        use std::time::Instant;
        let r = kod_types::redact::Redactor::default();
        let body = "    let value = compute(input).unwrap_or_else(|e| handle(e));\n";
        let mut text = String::new();
        while text.len() < 2048 {
            text.push_str(body);
        }
        let make = || -> Vec<ChatMessage> {
            (0..50)
                .map(|_| {
                    ChatMessage::text(
                        MessageId::new(),
                        MessageRole::User,
                        text.clone(),
                        time::OffsetDateTime::now_utc(),
                    )
                })
                .collect()
        };

        let mut msgs = make();
        let start = Instant::now();
        for _ in 0..40 {
            for m in msgs.iter_mut() {
                let (out, _) = r.redact(&m.content);
                m.content = out;
            }
        }
        let baseline = start.elapsed();

        let mut msgs = make();
        let mut m = memo();
        let start = Instant::now();
        for _ in 0..40 {
            let _ = redact_contents_memoised(&r, &mut msgs, &mut m);
        }
        let memoised = start.elapsed();

        println!("unmemoised 40 rounds: {baseline:?}");
        println!("memoised   40 rounds: {memoised:?}");
        println!(
            "ratio: {:.1}x",
            baseline.as_secs_f64() / memoised.as_secs_f64()
        );
    }
}
