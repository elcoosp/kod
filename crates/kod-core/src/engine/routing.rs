use super::*;

impl KodEngine {
    /// P7: drop endpoints whose declared trust tier does not clear
    /// `sensitivity`'s requirement.
    ///
    /// Pure and deterministic — the caller supplies the chain, the map
    /// of endpoint name → trust tier, and the sensitivity. The empty
    /// result means every endpoint failed the requirement; the caller
    /// decides whether to fall back to the unfiltered chain (the engine
    /// does, and logs a warning) or to refuse the turn. That decision
    /// is deliberately not made here so the policy lives in one place.
    ///
    /// `Sensitivity::Public` has no requirement, so the input is
    /// returned unchanged in that case.
    pub(crate) fn filter_chain_by_trust(
        chain: &[kod_provider::ModelRef],
        trust_map: &std::collections::HashMap<String, String>,
        sensitivity: kod_core_state::sensitivity::Sensitivity,
    ) -> Vec<kod_provider::ModelRef> {
        let req = kod_core_state::sensitivity::TrustRequirement::for_sensitivity(sensitivity);
        if req.0.is_none() {
            return chain.to_vec();
        }
        chain
            .iter()
            .filter(|m| {
                let tier = trust_map.get(&m.endpoint).map(String::as_str);
                req.satisfied_by(tier)
            })
            .cloned()
            .collect()
    }

    /// Record per-model metadata for an endpoint.
    ///
    /// Called by any path that has just fetched `list_models()` for
    /// `endpoint`. Entries merge: a second call with the same
    /// (endpoint, model) key replaces the previous value. Callers
    /// that never call this see the endpoint config's window, which
    /// is the pre-change behavior.
    pub fn record_model_catalog(&self, endpoint: &str, models: &[kod_provider::ModelInfo]) {
        if let Ok(mut guard) = self.model_catalog.write() {
            for m in models {
                guard.insert((endpoint.to_string(), m.id.clone()), m.clone());
            }
        }
    }

    /// Resolve the `(context_window, max_out_tokens)` hint for a
    /// model reference.
    ///
    /// Priority order: the per-model catalog entry's `context_window`
    /// (when present), then the endpoint config's `context_window`,
    /// then the built-in default. `max_out` is always the endpoint
    /// config's `max_tokens` — a model's generation cap is an
    /// endpoint-level setting in kod.
    pub(crate) fn budget_hint_for(&self, model_ref: &kod_provider::ModelRef) -> (usize, usize) {
        // The endpoint's max_tokens still comes from the endpoint
        // config (loaded from disk); the caller's `RouterConfig`
        // does not carry one.
        let max_out = match kod_config::KodConfig::load_cached() {
            Ok(cfg) => cfg
                .llm
                .endpoints
                .iter()
                .find(|e| e.name == model_ref.endpoint)
                .unwrap_or_else(|| cfg.llm.default_endpoint())
                .max_tokens
                .unwrap_or(2048),
            Err(_) => 2048,
        };

        // Tier 1: the *live* catalog. A provider's own reported
        // window (from `list_models`) is authoritative — it is the
        // number the server will actually enforce.
        if let Ok(guard) = self.model_catalog.read()
            && let Some(info) = guard.get(&(model_ref.endpoint.clone(), model_ref.model.clone()))
            && let Some(window) = info.context_window
        {
            return (window, max_out);
        }

        // Tier 2: the caller's `RouterConfig.context_window`. The
        // previous shape reloaded the user's on-disk config here, so
        // a caller that passed 8192 (a test, a small-model
        // deployment) got the user's 1M window instead of the value
        // it asked for. The caller's explicit value now wins over
        // the name-family heuristic and the built-in catalog — those
        // exist to *help* a caller that did not name a window, not to
        // override one that did.
        (self.config_window, max_out)
    }

    pub async fn set_registry(
        &self,
        registry: Arc<ProviderRegistry>,
        default_model: ModelRef,
        routing: Option<kod_config::RoutingConfig>,
    ) {
        *self.registry.write().await = Some(registry);
        // WS-A: when the active endpoint is a tab bridge, install the
        // engine's stable background session on every provider in the
        // registry, so all background traffic (extraction,
        // sharpshooter, compaction, judges, prewarm) shares one bridge
        // session — one background tab — instead of minting `anon-*`
        // per request. Main turns always carry an explicit session id,
        // which takes precedence over this default.
        self.install_background_session().await;
        // Cache the effective model's (window, max_out) before the
        // move into `current_model`. `budget_hint_for` consults the
        // per-model catalog when populated, then the endpoint config,
        // then the built-in default.
        let hint = self.budget_hint_for(&default_model);
        if let Ok(mut g) = self.budget_hint.write() {
            *g = hint;
        }
        // P7: retain each endpoint's trust tier so the routing gate
        // can drop an endpoint that does not meet the turn's
        // sensitivity requirement. An endpoint without a declared
        // tier is `standard`, matching `TrustRequirement`'s default.
        if let Ok(cfg) = kod_config::KodConfig::load_cached() {
            let mut trust = self.endpoint_trust.write().await;
            trust.clear();
            for ep in &cfg.llm.endpoints {
                trust.insert(
                    ep.name.clone(),
                    ep.trust.clone().unwrap_or_else(|| "standard".to_string()),
                );
            }
        }
        *self.current_model.write().await = default_model;
        *self.routing.write().await = routing;
    }

    /// Delta section 9.7: additional fallback candidates for a
    /// failure of `class` on `failed_selector`, resolved to
    /// `ModelRef`s via the registry and deduplicated against the
    /// endpoints already in `chain`.
    ///
    /// Empty when the retry config has no entry for this class (the
    /// default), when every candidate is already in the chain, or
    /// when a candidate's endpoint is not registered.
    pub(crate) async fn retry_chain_candidates(
        &self,
        failed_selector: &str,
        class: &str,
        chain: &[ModelRef],
    ) -> Vec<ModelRef> {
        let cfg = self.retry_config.read().await.clone();
        let names = cfg.resolve(failed_selector, class);
        if names.is_empty() {
            return Vec::new();
        }
        let registry = self.registry.read().await.clone();
        let Some(reg) = registry else {
            return Vec::new();
        };
        let mut out: Vec<ModelRef> = Vec::new();
        for endpoint in names {
            // Never re-add an endpoint the chain already holds (the
            // static chain, or a candidate added on an earlier
            // iteration).
            if chain.iter().any(|m| m.endpoint == endpoint)
                || out.iter().any(|m| m.endpoint == endpoint)
            {
                continue;
            }
            if let Some(model) = reg.default_model(&endpoint) {
                out.push(ModelRef::new(endpoint, model));
            }
        }
        out
    }

    /// Delta section 9.7: install the retry-fallback chains. Called
    /// by the CLI / TUI bootstrap from `config.llm.retry`.
    pub async fn set_retry_config(&self, retry: kod_config::RetryConfig) {
        *self.retry_config.write().await = retry;
    }

    /// WS-A: install [`Self::background_session_id`] as the default
    /// session on every provider in the registry, when (and only
    /// when) the active endpoint is a tab bridge. Idempotent: called
    /// from [`Self::set_registry`], and providers ignore it on
    /// non-tab endpoints (the default trait impl is a no-op and
    /// `OpenAICompatProvider` only stamps when asked).
    pub(crate) async fn install_background_session(&self) {
        if !self.tab_bridge_active().await {
            return;
        }
        let bg = self.background_session_id.to_prefixed_string();
        let registry = self.registry.read().await.clone();
        let Some(registry) = registry.as_ref() else {
            return;
        };
        for name in registry.names() {
            let model = registry.default_model(&name).unwrap_or_default();
            let model_ref = ModelRef::new(name.clone(), model);
            match registry.resolve(&model_ref) {
                Ok(provider) => provider.set_default_session(Some(bg.clone())),
                Err(e) => {
                    tracing::debug!(endpoint = %name, error = %e, "background session: resolve failed");
                }
            }
        }
        tracing::info!(session = %bg, "background session installed for tab-bridge endpoint");
    }

    /// Switch the current (endpoint, model). This is the TUI's
    /// `/model` operation: a `ModelRef` change without rebuilding the
    /// registry. A no-op when no registry is installed (the legacy
    /// `set_provider` path uses a provider whose model was baked in at
    /// construction).
    pub async fn set_current_model(&self, model_ref: ModelRef) {
        // Refresh the budget hint for the new model. A model whose
        // context window has been recorded in the catalog wins over
        // the endpoint config; a miss falls back to the endpoint
        // default, which is the same value the previous `/model`
        // switch would have left in place.
        let hint = self.budget_hint_for(&model_ref);
        if let Ok(mut g) = self.budget_hint.write() {
            *g = hint;
        }
        *self.current_model.write().await = model_ref;
    }

    /// The current (endpoint, model). Read by the TUI header and by
    /// `/model` completion.
    pub async fn current_model(&self) -> ModelRef {
        self.current_model.read().await.clone()
    }

    /// The USD pricing the registry associates with `model_ref`'s
    /// endpoint, if the endpoint carries a `[pricing]` block.
    ///
    /// Public so the CLI's `kod models` (and any future cost-report
    /// surface) can read the same number the engine uses when it
    /// populates `TaskResponse::pricing`.
    pub async fn pricing_for(&self, model_ref: &ModelRef) -> Option<kod_provider::ModelPricing> {
        // Tier 1: the endpoint's configured `[pricing]` block, carried
        // on the registry's capabilities. A user who set a price has
        // the last word.
        let reg = self.registry.read().await;
        if let Some(p) = reg
            .as_ref()
            .and_then(|r| r.capabilities(&model_ref.endpoint))
            .and_then(|c| c.pricing)
        {
            return Some(p);
        }
        drop(reg);
        // Tier 2: the live catalog (populated by `list_models`).
        if let Ok(guard) = self.model_catalog.read()
            && let Some(info) = guard.get(&(model_ref.endpoint.clone(), model_ref.model.clone()))
            && let (Some(i), Some(o)) = (info.input_per_mtok_usd, info.output_per_mtok_usd)
        {
            return Some(kod_provider::ModelPricing::new(i, o));
        }
        // Tier 3: the static built-in catalog. A rough default for a
        // well-known model whose endpoint carries no pricing block.
        // The user's config (`[endpoint.pricing]`) is authoritative
        // when present; the static table only fills the gap.
        if let Some(meta) = kod_provider::resolve_model_meta(&model_ref.model) {
            return Some(meta.pricing);
        }
        None
    }

    /// Compute the allocation for a prompt of `input.len()` chars
    /// against the configured endpoint's window. Reads the config
    /// each time; cheap (one file read at most) and correct after a
    /// `/model` switch that landed a different endpoint.
    pub(crate) async fn prompt_allocation(
        &self,
        input: &str,
        _history: &str,
    ) -> std::result::Result<kod_core_state::budget::Allocation, kod_core_state::budget::BudgetError>
    {
        // Hygiene: read the cached (window, max_out). `set_registry`
        // populates it from the effective endpoint; a caller that
        // never installed a registry sees the built-in default.
        let (window, max_out) = *self.budget_hint.read().unwrap_or_else(|e| e.into_inner());
        let budget = kod_core_state::budget::PromptBudget::from_tokens(window, max_out);
        // H-E3: subtract the parts the engine appends outside the four
        // budgeted sections — the environment/tool-use trailer, the
        // structured system prompt, and the tool JSON schemas. The
        // tool schemas are the bulk; measure them once.
        let overhead = {
            let defs = self.tools.get_definitions().await;
            let schemas = defs
                .iter()
                .map(|d| d.parameters_schema.to_string().len() + d.name.len() + d.description.len())
                .sum::<usize>();
            // Trailer (~512 chars) + a conservative system-prompt
            // estimate (~2048) so a prompt with no schema still
            // reserves some room for the mandatory trailer.
            512 + 2048 + schemas
        };
        // Cap the overhead at 3/4 of the budget. Without this, a large
        // tool set (many schemas) can exceed the whole budget on a
        // small-window model, and `allocate_with_overhead` returns an
        // error that fails *every* turn — a fat tool set bricks the
        // session. The cap leaves a quarter of the budget for the
        // request and the truncatable sections. A genuinely oversized
        // request still errors (that is the design's intent); what we
        // no longer allow is overhead alone consuming the window.
        let capped_overhead = overhead.min(budget.total_chars * 3 / 4);
        if capped_overhead < overhead {
            tracing::warn!(
                overhead,
                capped = capped_overhead,
                total = budget.total_chars,
                "tool-schema overhead exceeds 3/4 of the prompt budget;                  capping it — reduce the enabled tool set or raise the                  endpoint's context_window",
            );
        }
        budget.allocate_with_overhead(input.len(), capped_overhead)
    }

    /// The ordered `ModelRef` chain for a task type. The first element
    /// is the endpoint `routing.by_task` names, or `current_model` when
    /// there is no routing table for this task. The rest are the
    /// entries in `routing.fallback`, deduplicated against the primary
    /// and each other. Empty when no provider is available at all.
    ///
    /// A v1 config (`routing = None`) produces a single-element chain
    /// containing `current_model`, so the fallback loop degenerates to
    /// a single attempt — identical to the pre-A6 behaviour.
    /// Streaming chain for a turn. The override, when present,
    /// replaces the *primary* endpoint with a caller-chosen
    /// `ModelRef` (the swarm runner's per-capability routing).
    /// `[llm.routing].fallback` endpoints are still appended so a
    /// fallback chain survives the override — the override is a
    /// routing decision, not a fallback-free mandate.
    ///
    /// `None` delegates to `resolve_chain_for_task`, the pre-A7
    /// behaviour. Both paths return an empty vec when no provider is
    /// reachable; the caller reports "no provider".
    pub(crate) async fn build_streaming_chain(
        &self,
        task_key: &str,
        override_model: Option<&ModelRef>,
    ) -> Vec<ModelRef> {
        match override_model {
            None => self.resolve_chain_for_task(task_key).await,
            Some(primary) => {
                let mut chain = vec![primary.clone()];
                let routing = self.routing.read().await.clone();
                if let Some(r) = &routing {
                    let registry = self.registry.read().await.clone();
                    if let Some(reg) = registry {
                        for endpoint in &r.fallback {
                            if chain.iter().any(|c| c.endpoint == *endpoint) {
                                continue;
                            }
                            if let Some(model) = reg.default_model(endpoint) {
                                chain.push(ModelRef::new(endpoint.clone(), model));
                            }
                        }
                    }
                }
                chain
            }
        }
    }

    /// Resolve a swarm subtask's capability to a `ModelRef` via
    /// `[llm.routing.swarm]`. `None` when the config has no swarm
    /// table, no entry for this capability, or the endpoint is not
    /// registered — every one of which means "route by task type",
    /// the pre-A7 behaviour.
    ///
    /// Public so the swarm runner (which holds an `Arc<KodEngine>`
    /// and no config) can query the mapping before each subtask.
    pub async fn resolve_model_ref_for_capability(
        &self,
        capability: &kod_swarm::Capability,
    ) -> Option<ModelRef> {
        let routing = self.routing.read().await.clone()?;
        let endpoint = routing.swarm.get(capability.as_str())?;
        let registry = self.registry.read().await.clone()?;
        registry
            .default_model(endpoint)
            .map(|model| ModelRef::new(endpoint.clone(), model))
    }

    /// Resolve the endpoint chain, with a cache-awareness gate when
    /// the ledger knows a warm endpoint (P1).
    ///
    /// The `by_task` order is preserved — the ledger does not
    /// reorder or re-rank. What it does is insert the warm endpoint
    /// at position 0 when the classification's first choice would
    /// be cold and the hop is not worth its switch penalty. The rest
    /// of the chain is untouched, so a fallback failure still walks
    /// the same endpoints in the same order.
    ///
    /// The gate needs the head fingerprint and the transcript size,
    /// both of which are computed per turn; this method reads them
    /// from the current request state. A caller that resolves the
    /// chain before the request is prepared sees the pre-P1 shape.
    pub(crate) async fn resolve_chain_for_task_gated(
        &self,
        task_key: &str,
        head_fingerprint: u64,
        transcript_tokens: u64,
    ) -> Vec<ModelRef> {
        let mut chain = self.resolve_chain_for_task(task_key).await;
        // P7: filter the chain by the current turn's sensitivity
        // before the cache gate. An endpoint whose declared trust
        // tier is below the requirement is dropped; if that leaves
        // the chain empty, the *unfiltered* chain is restored and a
        // warning logged — a mis-configured config that marks every
        // endpoint `untrusted` must not brick the session, and a
        // turn is better served by a wrong-tier endpoint than by no
        // endpoint at all.
        {
            let sensitivity = *self.current_sensitivity.read().await;
            let trust_map = self.endpoint_trust.read().await;
            let filtered = Self::filter_chain_by_trust(&chain, &trust_map, sensitivity);
            if filtered.is_empty() && !chain.is_empty() {
                tracing::warn!(
                    sensitivity = sensitivity.label(),
                    endpoints = chain.len(),
                    "no endpoint meets the sensitivity requirement; using \
                     the unfiltered chain",
                );
            } else if !filtered.is_empty() {
                chain = filtered;
            }
        }
        // Hygiene 3.2: drop endpoints whose circuit breaker is open.
        // Runs before the cache gate — an endpoint that is being
        // skipped for health should not be considered for warmth.
        if let Ok(mut h) = self.endpoint_health.lock() {
            chain = h.filter_chain(&chain, |m| m.endpoint.as_str());
        }
        if chain.len() < 2 {
            return chain;
        }
        let preferred = chain[0].clone();
        // The endpoint the ledger believes is warm, if it is also in
        // the chain. Anything else is not a candidate for the gate —
        // the ledger will not route to an endpoint the
        // classification did not choose.
        let sticky = self
            .cache_ledger
            .lock()
            .ok()
            .and_then(|l| l.sticky().map(str::to_string));
        let Some(sticky) = sticky else {
            return chain;
        };
        if sticky == preferred.endpoint {
            return chain;
        }
        let sticky_idx = match chain.iter().position(|m| m.endpoint == sticky) {
            Some(i) => i,
            None => return chain,
        };
        // Projected penalty of using `preferred` on this turn's
        // prefix (cold → re-process the whole transcript).
        let preferred_pricing = self.pricing_for(&preferred).await;
        let penalty = match preferred_pricing {
            Some(p) => self
                .cache_ledger
                .lock()
                .ok()
                .map(|l| {
                    l.switch_penalty_usd(
                        &preferred.endpoint,
                        head_fingerprint,
                        &p,
                        transcript_tokens,
                    )
                })
                .unwrap_or(0.0),
            // No pricing → cannot estimate; treat as zero penalty so
            // the classification's choice is honoured (local
            // endpoints have no cost, and the switch is free).
            None => 0.0,
        };
        // The per-turn saving the hop would win is the difference in
        // input-price rates applied to the transcript. This is a
        // lower bound: it ignores output-price differences and
        // cache-read differences. A caller that wants a richer
        // estimate should pass one in; the ledger's job is to gate,
        // not to model.
        let per_turn_saving = match (
            self.pricing_for(&preferred).await,
            self.pricing_for(&chain[sticky_idx]).await,
        ) {
            (Some(a), Some(b)) => {
                let m = 1_000_000.0;
                let diff = b.input_per_mtok_usd - a.input_per_mtok_usd;
                (transcript_tokens as f64 / m) * diff.max(0.0)
            }
            _ => 0.0,
        };
        let chosen = self
            .cache_ledger
            .lock()
            .ok()
            .map(|l| {
                l.gate(&preferred.endpoint, &sticky, per_turn_saving, penalty)
                    .to_string()
            })
            .unwrap_or_else(|| preferred.endpoint.clone());
        if chosen == sticky {
            // Hop declined; move the warm endpoint to the front of
            // the chain. The preferred endpoint stays as a fallback
            // at its original position.
            let warm = chain.remove(sticky_idx);
            chain.insert(0, warm);
        }
        chain
    }

    pub(crate) async fn resolve_chain_for_task(&self, task_key: &str) -> Vec<ModelRef> {
        // Legacy path: no registry.
        if self.registry.read().await.is_none() {
            return vec![self.current_model.read().await.clone()];
        }
        let registry = {
            let g = self.registry.read().await;
            g.as_ref().map(Arc::clone).unwrap()
        };
        let routing = self.routing.read().await.clone();

        // Delta §11.10: a transcript in plan mode routes through the
        // `[llm.routing].plan` endpoint when one is configured. This
        // is the plan-model role: a cheap-and-fast model to plan, a
        // stronger one to execute (or the reverse). A plan-mode turn
        // whose configured endpoint is not registered falls through
        // to the ordinary chain, so a typo in the config cannot make
        // planning unusable.
        //
        // The switch is *deferred to the turn boundary* — this method
        // is called once per turn before any streaming starts, so a
        // mid-stream `/plan-mode` cannot swap the model under the
        // current stream. That is the design's rule.
        if self.is_in_plan_mode(DEFAULT_TRANSCRIPT_KEY).await
            && let Some(r) = &routing
            && let Some(endpoint) = &r.plan
            && let Some(model) = registry.default_model(endpoint)
        {
            return vec![ModelRef::new(endpoint.clone(), model)];
        }

        let mut chain: Vec<ModelRef> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

        // P5.2 — Jev's dynamic routing decision, when available,
        // goes first. The static table's entry, if any, still
        // appears in the chain as a fallback.
        if let Some(primary) = self.pick_endpoint_with_jev(task_key).await
            && seen.insert(primary.endpoint.clone())
        {
            chain.push(primary);
        }

        if let Some(r) = &routing
            && let Some(primary) = r.by_task.get(task_key)
            && let Some(model) = registry.default_model(primary)
            && seen.insert(primary.clone())
        {
            chain.push(ModelRef::new(primary.clone(), model));
        }

        if let Some(r) = &routing {
            for endpoint in &r.fallback {
                if !seen.insert(endpoint.clone()) {
                    continue;
                }
                if let Some(model) = registry.default_model(endpoint) {
                    chain.push(ModelRef::new(endpoint.clone(), model));
                }
            }
        }

        // No routing, or routing that pointed at unregistered
        // endpoints: fall back to current_model as the sole entry.
        if chain.is_empty() {
            chain.push(self.current_model.read().await.clone());
        }
        chain
    }

    /// Resolve a `ModelRef` to a provider. Registry-based when a
    /// registry is installed; the legacy single-provider slot
    /// otherwise (any `ModelRef` resolves to it).
    pub(crate) async fn resolve_provider_for_model_ref(
        &self,
        model_ref: &ModelRef,
    ) -> Result<Arc<dyn LlmProvider>> {
        match self.registry.read().await.as_ref() {
            Some(registry) => registry.resolve(model_ref),
            None => Err(Self::no_provider_error()),
        }
    }

    /// Resolve the `judge` role to a [`kod_provider::judgment::JudgmentClient`],
    /// when one is configured.
    ///
    /// # Why this exists
    ///
    /// `AutoThinking` (delta §9.6) and the `UnexpectedStopClassifier`
    /// (§9.4) are both `JudgmentClient` consumers. The client is
    /// constructed from a `(provider, model_ref, options)` triple, but
    /// nothing in the engine resolved a *role name* into that triple —
    /// so the two classifiers were testable only against a manually
    /// constructed client (see `auto_thinking.rs` and
    /// `unexpected_stop.rs` test fixtures). This method is the missing
    /// bridge: it turns the config-level `judge` role into a client the
    /// engine can hand to either classifier.
    ///
    /// # Where the role is looked up
    ///
    /// `RoutingConfig` carries two maps. `swarm` is role-addressed —
    /// the natural home for a `"judge"` entry — and is checked first.
    /// `by_task` is task-shaped (`"Simple"`, `"Debugging"`) and is a
    /// reasonable second home for a caller who thinks of the judge as
    /// a routing task. Either location suffices.
    ///
    /// # No fallback to `current_model`
    ///
    /// Unlike [`Self::resolve_chain_for_task`], this deliberately does
    /// **not** fall back to the current model when no `judge` role is
    /// configured. The design's premise (§9.5) is that the judge is a
    /// *different* model from the one being judged — a judge that is
    /// the same model as the producer provides no independent signal
    /// and, on a small local model, is actively harmful (it will agree
    /// with itself). Absence of a `judge` role therefore means
    /// "judging is disabled", and the caller's contract is to fall
    /// back to whatever behaviour it had before the judge path
    /// existed. See `auto_thinking.rs`'s keyword fallback and
    /// `unexpected_stop.rs`'s "most turns are not candidates" gate.
    ///
    /// Returns `None` in four cases: no routing config, no `judge`
    /// entry in either map, an endpoint name that the registry does
    /// not know, or a legacy single-provider engine with no registry.
    pub async fn resolve_judge_client(&self) -> Option<kod_provider::judgment::JudgmentClient> {
        let routing = self.routing.read().await.clone();
        let routing = routing.as_ref()?;

        let endpoint_name = routing
            .swarm
            .get("judge")
            .or_else(|| routing.by_task.get("judge"))?
            .clone();

        let registry = {
            let g = self.registry.read().await;
            g.as_ref().map(Arc::clone)?
        };

        let model_name = registry.default_model(&endpoint_name)?;
        let model_ref = ModelRef::new(endpoint_name, model_name);
        let provider = registry.resolve(&model_ref).ok()?;

        Some(kod_provider::judgment::JudgmentClient::new(
            provider,
            model_ref,
            kod_provider::judgment::JudgmentOptions::default(),
        ))
    }

    /// The reasoning-effort ladder a model supports, from the live
    /// catalog.
    ///
    /// Falls back to `[Medium]` when the catalog has no entry, the
    /// entry has no `efforts` field, or the field is an empty vec.
    /// That is the same default `ModelInfo::efforts`'s doc names, and
    /// it is what makes `AutoThinking` collapse to the neutral level
    /// for a provider that does not report a ladder — the classifier
    /// has only one label to pick, so the answer is deterministically
    /// `Medium`.
    fn supported_efforts_for(&self, model_ref: &ModelRef) -> Vec<kod_types::effort::EffortLevel> {
        if let Ok(guard) = self.model_catalog.read()
            && let Some(info) = guard.get(&(model_ref.endpoint.clone(), model_ref.model.clone()))
            && let Some(ladder) = info.efforts.as_ref()
            && !ladder.is_empty()
        {
            return ladder.clone();
        }
        vec![kod_types::effort::EffortLevel::Medium]
    }

    /// Delta §9.6: resolve the reasoning effort for one turn.
    ///
    /// Precedence, strongest first:
    ///
    /// 1. An explicit caller effort (`Some` on the `effort` parameter
    ///    of `process_streaming_with_model_for`). A caller that named
    ///    a level gets that level; the config cannot override it.
    /// 2. The endpoint config's `effort` field, when present and not
    ///    the `"auto"` sentinel. A fixed string is parsed through
    ///    `EffortLevel::parse`, which returns `Medium` for an
    ///    unrecognized value.
    /// 3. When the config says `"auto"`: build a judge client from
    ///    the `judge` role (`resolve_judge_client`), look up the
    ///    model's ladder (`supported_efforts_for`), and classify the
    ///    turn's input. The classifier clamps to the ladder and
    ///    ceilings the auto answer one tier below the top, per the
    ///    design.
    /// 4. When no `judge` role is configured, or classification
    ///    fails: `None`, which leaves `options.effort` at whatever
    ///    the generation defaults supplied. Judging is optional; a
    ///    session without a `judge` role simply does not get the
    ///    LLM-judge effort decision, and every other behaviour is
    ///    unchanged.
    ///
    /// # Why a judge failure is silent
    ///
    /// A judge hiccup (a network blip, a model that replied in
    /// prose) must not fail the turn. The classifier itself already
    /// maps an unparseable reply to `Medium`; the `Err` arm here is
    /// for the harder failure shapes (provider error, timeout) and
    /// logs at `warn` for a developer reading a trace, then falls
    /// through to the caller's default. The alternative — failing
    /// every turn on a judge hiccup — would make `effort = "auto"` a
    /// liability rather than a feature.
    pub(crate) async fn resolve_turn_effort(
        &self,
        input: &str,
        model_ref: &ModelRef,
        caller_effort: Option<kod_types::effort::EffortLevel>,
    ) -> Option<kod_types::effort::EffortLevel> {
        if caller_effort.is_some() {
            return caller_effort;
        }

        let config_effort = match kod_config::KodConfig::load_cached() {
            Ok(cfg) => cfg
                .llm
                .endpoints
                .iter()
                .find(|e| e.name == model_ref.endpoint)
                .unwrap_or_else(|| cfg.llm.default_endpoint())
                .effort
                .clone(),
            Err(_) => None,
        };
        let config_effort = config_effort?;

        if config_effort != "auto" {
            return Some(kod_types::effort::EffortLevel::parse(&config_effort));
        }

        // The `"auto"` sentinel. Build a judge.
        let client = match self.resolve_judge_client().await {
            Some(c) => c,
            None => {
                tracing::debug!(
                    endpoint = %model_ref.endpoint,
                    "effort = \"auto\" but no judge role is configured; leaving effort unset",
                );
                return None;
            }
        };
        let supported = self.supported_efforts_for(model_ref);
        let classifier = kod_core_quality::auto_thinking::AutoThinking::new(client);
        match classifier.classify(input, &supported).await {
            Ok(level) => {
                tracing::debug!(
                    effort = level.as_str(),
                    endpoint = %model_ref.endpoint,
                    "auto-thinking classified the turn",
                );
                Some(level)
            }
            Err(e) => {
                tracing::warn!(
                    endpoint = %model_ref.endpoint,
                    error = %e,
                    "auto-thinking judge failed; leaving effort at the config default",
                );
                None
            }
        }
    }

    /// Delta §9.4 (diagnostic half): classify whether a clean stop
    /// was actually an answer.
    ///
    /// This is the wiring that makes the `UnexpectedStopClassifier`
    /// observable from the engine. It runs on every completed turn,
    /// but only *evaluates* on the candidate pattern (a clean stop
    /// with non-empty text and no tool calls) — most turns cost
    /// nothing more than a `StopCandidate` build and a `is_candidate`
    /// check. A turn that is a candidate, with a `judge` role
    /// configured, spends exactly one judge call.
    ///
    /// # Diagnostic-only, for now
    ///
    /// The classifier's verdict is **logged** — at `info` for an
    /// unexpected stop, at `debug` for an expected one, at `warn` for
    /// a judge failure — but the engine takes no corrective action on
    /// it in this commit. Extending the turn (injecting a synthetic
    /// nudge and re-entering the streaming chain) is a distinct
    /// policy decision that touches the streaming loop's control
    /// flow, and it deserves its own commit. Landing the diagnostic
    /// first means the verdict is visible in traces and can be
    /// measured against real sessions before it can affect them.
    ///
    /// # Where the stop reason comes from
    ///
    /// The engine does not currently thread the provider's
    /// `StopReason` chunk out of the streaming loop. Passing `None`
    /// is correct: `StopCandidate.stop_reason` treats `None` as
    /// "consistent with a clean stop" — the very shape the
    /// classifier was built to judge. Once the loop's stop reason
    /// is threaded through (a small follow-up), the caller upgrades
    /// to `Some(reason)` and the "not a truncation detector"
    /// property in the module doc becomes an active gate rather
    /// than a doc invariant.
    pub(crate) async fn diagnose_unexpected_stop(
        &self,
        holder: &str,
        request: &str,
        reply: &str,
        tool_call_count: usize,
    ) {
        use crate::unexpected_stop::{StopCandidate, UnexpectedVerdict, is_candidate};

        let candidate = StopCandidate {
            stop_reason: None,
            text: reply,
            has_tool_calls: tool_call_count > 0,
        };
        if !is_candidate(&candidate) {
            return;
        }

        let client = match self.resolve_judge_client().await {
            Some(c) => c,
            None => {
                tracing::debug!(
                    holder = %holder,
                    "unexpected-stop candidate but no judge role configured",
                );
                return;
            }
        };
        let classifier = crate::unexpected_stop::UnexpectedStopClassifier::new(client);
        match classifier.classify(request, reply).await {
            Ok(true) => tracing::info!(
                holder = %holder,
                verdict = ?UnexpectedVerdict::Unexpected,
                "unexpected stop detected (diagnostic only; no corrective emitted)",
            ),
            Ok(false) => tracing::debug!(
                holder = %holder,
                verdict = ?UnexpectedVerdict::Expected,
                "stop classified as expected",
            ),
            Err(e) => tracing::warn!(
                holder = %holder,
                error = %e,
                "unexpected-stop judge failed",
            ),
        }
    }

    /// Like [`Self::record_cost`], but also feeds the cache ledger
    /// with the fingerprint of the request head that was actually
    /// sent. The engine calls this from the two loops, which have
    /// the rendered `system_text` and the definitions in scope.
    pub(crate) async fn record_cost_with_head(
        &self,
        holder: &str,
        model_ref: &ModelRef,
        usage: &kod_provider::TokenUsage,
        pricing: Option<kod_provider::ModelPricing>,
        head_fingerprint: u64,
    ) {
        // Feed the ledger first — it is cheap and the value is
        // useful even if the log write is skipped because no
        // recorder is installed.
        self.ledger_observe(&model_ref.endpoint, head_fingerprint, usage);

        // Delta §2.4: anchor the per-transcript context gauge on this
        // settled usage. `covers_through` is the last index the
        // provider charged for; since `record_cost_with_head` runs
        // immediately after the stream completes and before any
        // mutation of `self.history[holder]`, the history's current
        // length is exactly the message count that was sent. A
        // caller with an unusual transcript-mutation shape that
        // breaks this invariant would over- or under-count the tail
        // by the difference, which is a bug in that caller, not here.
        {
            let covers_through = self
                .history
                .read()
                .await
                .get(holder)
                .map(|turns| turns.len().saturating_sub(1))
                .unwrap_or(0);
            self.gauge_observe(holder, covers_through, usage).await;
        }

        // Delta §13.2: one RequestRecord per provider call. Rates come
        // from the endpoint pricing when present; a local endpoint with
        // no pricing records zero rates, which makes cache_savings_usd
        // zero rather than a guess.
        {
            let (input_rate, cache_read_rate, cache_write_rate) = match pricing {
                Some(p) => (
                    p.input_per_mtok_usd,
                    p.cache_read_per_mtok_usd,
                    p.cache_write_per_mtok_usd,
                ),
                None => (0.0, 0.0, 0.0),
            };
            let record = kod_stats::request::RequestRecord {
                model: model_ref.model.clone(),
                endpoint: model_ref.endpoint.clone(),
                duration_ms: 0,
                ttft_ms: None,
                stop_reason: None,
                input_tokens: usage.prompt_tokens as u64,
                output_tokens: usage.completion_tokens as u64,
                cache_read_tokens: usage.cache_read_tokens.unwrap_or(0),
                cache_write_tokens: usage.cache_creation_tokens.unwrap_or(0),
                input_rate,
                cache_read_rate,
                cache_write_rate,
                error: false,
            };
            self.stats.lock().observe(&record);
        }

        // Delta §14.5: mirror the turn completion into OTLP when a
        // telemetry handle is installed. The `try_read` keeps the
        // async-turn path from blocking on a writer; a missed record
        // is not a correctness issue.
        if let Ok(t) = self.telemetry.try_read() {
            t.record_turn(kod_telemetry::TurnRecord {
                system: model_ref.endpoint.clone(),
                model: model_ref.model.clone(),
                endpoint: model_ref.endpoint.clone(),
                prompt_tokens: usage.prompt_tokens as u64,
                completion_tokens: usage.completion_tokens as u64,
                cache_read_tokens: usage.cache_read_tokens.unwrap_or(0),
                cache_creation_tokens: usage.cache_creation_tokens.unwrap_or(0),
                ttft_ms: None,
                duration_ms: 0,
                stop_reason: None,
                error: false,
            });
        }

        // P2-a: record the observed size for the compaction decision.
        // `prompt + completion` is the whole window the provider
        // processed; that is what the next request will roughly
        // repeat before new turns are appended.
        let observed = usage.prompt_tokens.saturating_add(usage.completion_tokens) as u64;
        self.observed_usage
            .write()
            .await
            .insert(holder.to_string(), observed);
        // Delegate the accounting to the base method; it will feed
        // the ledger again with a zero fingerprint, which is a
        // no-op overwrite of the correct value just written. (The
        // ledger's `observe` replaces the state on every call; the
        // zero-fingerprint pass only clears it, so do the accounting
        // inline here rather than risk a second overwrite.)

        // The session-log cost line is the only part of this method
        // that needs pricing. A local endpoint with no `[pricing]`
        // block still gets its usage recorded (context gauge, cache
        // ledger, observed-usage) — only the cost line is skipped.
        if let Some(pricing) = pricing
            && let Ok(guard) = self.session_recorder.read()
            && let Some(rec) = guard.as_ref()
        {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let cost = pricing.cost_for_usage(usage);
            self.cost_tracker.record(cost);
            let entry = kod_core_state::session_log::SessionEntry::Cost {
                timestamp_ms: now_ms,
                holder: holder.to_string(),
                endpoint: model_ref.endpoint.clone(),
                model: model_ref.model.clone(),
                prompt_tokens: usage.prompt_tokens,
                completion_tokens: usage.completion_tokens,
                cost_usd: cost,
            };
            if let Err(e) = rec.record(&entry) {
                tracing::warn!(error = %e, "could not append Cost to session log");
            }
        }
    }

    /// Delta §14.5: append a `SessionExit` marker. Called from
    /// `shutdown` with the in-flight tool calls (usually empty), so a
    /// resume can tell a clean stop from a crash: a session with tool
    /// starts and no exit marker was interrupted.
    pub(crate) fn record_session_exit(&self, reason: &str, pending_tool_calls: Vec<String>) {
        if let Ok(guard) = self.session_recorder.read()
            && let Some(rec) = guard.as_ref()
        {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let entry = kod_core_state::session_log::SessionEntry::SessionExit {
                timestamp_ms: now_ms,
                holder: DEFAULT_TRANSCRIPT_KEY.to_string(),
                reason: reason.to_string(),
                pending_tool_calls,
            };
            if let Err(e) = rec.record(&entry) {
                tracing::warn!(error = %e, "could not append SessionExit to session log");
            }
        }
    }

    /// Delta §14.5: append a `ToolExecutionStart`. Pairs with the later
    /// `ToolCall` completion; an unmatched start is what a resume
    /// reports as an interrupted call.
    pub(crate) fn record_tool_execution_start(
        &self,
        holder: &str,
        tool_name: &str,
        call_id: Option<&str>,
    ) {
        if let Ok(guard) = self.session_recorder.read()
            && let Some(rec) = guard.as_ref()
        {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let entry = kod_core_state::session_log::SessionEntry::ToolExecutionStart {
                timestamp_ms: now_ms,
                holder: holder.to_string(),
                tool_name: tool_name.to_string(),
                call_id: call_id.map(String::from),
            };
            if let Err(e) = rec.record(&entry) {
                tracing::warn!(error = %e, "could not append ToolExecutionStart to session log");
            }
        }
    }

    /// Append a `SessionEntry::ModelFallback` to the recorder, if one is
    /// installed. Best-effort: a write failure logs and the run
    /// continues.
    pub(crate) async fn record_model_fallback(
        &self,
        holder: &str,
        from: &ModelRef,
        to: &ModelRef,
        error: &str,
    ) {
        if let Ok(guard) = self.session_recorder.read()
            && let Some(rec) = guard.as_ref()
        {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let entry = kod_core_state::session_log::SessionEntry::ModelFallback {
                timestamp_ms: now_ms,
                holder: holder.to_string(),
                from: from.display(),
                to: to.display(),
                error: error.to_string(),
            };
            if let Err(e) = rec.record(&entry) {
                tracing::warn!(error = %e, "could not append ModelFallback to session log");
            }
        }
    }

    /// Error for a `process*` call made before a provider is installed.
    ///
    /// The router has a set of placeholder handlers that return
    /// strings like "Processing simple task: …". Those exist so the
    /// router's own unit tests can exercise the classification path
    /// without a provider, and they are fine in that role. As a
    /// user-visible answer from the engine, though, they are worse
    /// than an error: the call looks like it succeeded, the reply
    /// advertises no fix, and the operator has to guess that the
    /// engine was never wired to a model.
    ///
    /// Callers that do want the placeholder behavior (the router's
    /// own tests) use `TaskRouter` directly. The engine tells the
    /// truth.
    pub(crate) fn no_provider_error() -> KodError {
        KodError::InvalidState(
            "No LLM provider configured. Install one with \
             `engine.install_test_provider(Arc::new(provider))` before calling \
             process — kod-cli and kod-tui do this automatically from \
             ~/.config/kod/config.toml."
                .to_string(),
        )
    }

    /// Delta §7.5: store a blob of text as an artifact and return the
    /// `artifact://<id>` URL the model can `read_file`.
    ///
    /// This is the entry point for any code that wants to offload
    /// text out of the transcript — shake, the shell-output
    /// minimizer, a large tool result. The URL is stable; the bytes
    /// are immutable; a later turn that re-reads the URL sees the
    /// same content.
    ///
    /// The id is caller-supplied. A caller that wants uniqueness
    /// generates one (a UUID, a content hash); a caller that wants
    /// to fail on collision passes a deterministic id. The
    /// underlying store refuses to overwrite.
    pub async fn store_artifact(
        &self,
        id: impl Into<String>,
        text: impl Into<String>,
        mime: impl Into<String>,
    ) -> Result<String> {
        self.artifact_handler
            .store(id, text, mime)
            .await
            .map_err(|e| KodError::InvalidParameters {
                reason: format!("store_artifact: {e}"),
            })
    }

    /// The artifact handler the internal-URL router dispatches to.
    /// Public so a caller that wants to preload artifacts before a
    /// session, or inspect the store, can reach it directly.
    pub fn artifact_handler(&self) -> &Arc<kod_tools::ArtifactHandler> {
        &self.artifact_handler
    }

    /// Execute a registered tool by name with the engine's own tool
    /// context. Used by the swarm runner's repo probe; a caller that
    /// wants a specific tool can also reach it this way, but the
    /// agentic loop is the ordinary path.
    pub async fn run_tool(
        &self,
        name: &str,
        args: serde_json::Value,
    ) -> Result<kod_types::ToolResult> {
        // P6: a background job's engine refuses any tool outside the
        // read-only whitelist. Enforcement is here, at the call, not
        // by a prompt — a prompt cannot be trusted to hold.
        if self
            .background_mode
            .load(std::sync::atomic::Ordering::Relaxed)
            && !crate::background::is_read_only(name)
        {
            return Ok(kod_types::ToolResult::Error(format!(
                "policy denied: `{name}` is not on the background read-only \
                 whitelist. Allowed: {}",
                crate::background::READ_ONLY_TOOLS.join(", "),
            )));
        }
        self.tools
            .execute_tool(name, &args, &self.tool_context)
            .await
    }

    /// Mark this engine as a background job's child (P6). Idempotent;
    /// a caller that constructs a child engine sets this before
    /// handing it to a job.
    pub fn enable_background_mode(&self) {
        self.background_mode
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Whether this engine is in background mode.
    pub fn is_background(&self) -> bool {
        self.background_mode
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// List available models from the configured provider.
    ///
    /// Returns `Ok(vec![])` when no provider is set — there really
    /// are zero models to list, and an empty list is the correct
    /// answer. Returns `Err(...)` when a provider *is* set but the
    /// request to it fails — the answer is unknown (server down, auth
    /// wrong, endpoint mistyped), and collapsing that into an empty
    /// vec makes a caller unable to distinguish "the server has no
    /// models" from "the server is not reachable." The two deserve
    /// different user-facing messages and different recovery paths.
    pub async fn list_models(&self) -> Result<Vec<kod_provider::ModelInfo>> {
        match self.current_provider().await {
            Some(p) => p.list_models().await,
            None => Ok(Vec::new()),
        }
    }
}
