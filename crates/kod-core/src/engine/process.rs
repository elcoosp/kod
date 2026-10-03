use super::*;

impl KodEngine {
    /// Process user input
    pub async fn process(&self, input: &str) -> Result<TaskResponse> {
        self.process_for(DEFAULT_TRANSCRIPT_KEY, input).await
    }

    /// Process user input on a named transcript. `key` selects which
    /// transcript (and last-prompt slot) this call reads and writes.
    /// The swarm runner passes `swarm:<agent-id>` per agent, so three
    /// concurrent agents do not interleave their turns.
    /// S10: build the model-facing prompt from the classified
    /// response. Steps (all identical across the three process_*
    /// entry points):
    ///
    /// 1. Compute the per-section budget and modulate its split with
    ///    Jev's per-round read.
    /// 2. Build the prompt against the router's context builder.
    /// 3. Filter the tool definitions through Jev (category filter),
    ///    then trim the MCP half.
    /// 4. Ground the prompt (system preamble + tool inventory).
    ///
    /// Returns the allocation (for the caller's prompt trace), the
    /// tool definitions the model will see, and the grounded prompt
    /// text. The goal path appends its goal block to the returned
    /// string.
    pub(crate) async fn build_budgeted_prompt(
        &self,
        key: &str,
        input: &str,
        task_type: crate::router::TaskType,
        history: &str,
        memory_context: Option<kod_types::MemoryContext>,
    ) -> Result<(
        std::result::Result<crate::budget::Allocation, crate::budget::BudgetError>,
        Vec<kod_types::ToolDefinition>,
        String,
    )> {
        let base_alloc = self.prompt_allocation(input, history).await;
        // P2.1 — modulate the fixed 50/20/20/10 shares with Jev's
        // per-round read. The total is preserved; only the split
        // changes.
        let alloc = match &base_alloc {
            Ok(a) => Ok(self.reallocate_with_jev(key, input, a).await),
            Err(e) => Err(*e),
        };
        let prompt = match &alloc {
            Ok(a) => {
                self.router
                    .build_prompt_with_budget(input, &task_type, history, memory_context, Some(a))
                    .await?
            }
            Err(e) => {
                return Err(kod_error::KodError::InvalidParameters {
                    reason: e.to_string(),
                });
            }
        };

        // Ground the model: where it runs and what it can touch.
        //
        // P3: the per-turn category filter is retired when
        // `tool_search` is registered. The filter existed to keep the
        // tools array small; `tool_search` makes that unnecessary
        // because the model can pull schemas on demand. The filter
        // was also cache-hostile — tools sit in the cached prefix,
        // so a per-turn flip invalidated the whole prefix (P0
        // hysteresis mitigated that; retiring the filter eliminates
        // the cause).
        //
        // The underlying Jev call is a no-op when no Jev client is
        // installed, so this changes no golden-prompt bytes. A kod
        // build without `tool_search` (a minimal embedder) keeps the
        // filter as a fallback.
        let has_tool_search = self.tools.has("tool_search").await;
        let (definitions, _filter_changed) = if has_tool_search {
            (self.tools.get_definitions().await, false)
        } else {
            self.filter_tool_definitions_with_hysteresis(
                key,
                input,
                task_type,
                self.tools.get_definitions().await,
            )
            .await
        };
        // P5.1 — trim the MCP half of the tool list. Same reasoning:
        // `tool_search` surfaces MCP tools on demand, so the per-turn
        // trim is skipped when it is present.
        let definitions = if has_tool_search {
            definitions
        } else {
            self.filter_mcp_tools_with_jev(key, input, definitions)
                .await
        };
        // Delta §11.10: plan-mode subagent clamp. A transcript in
        // plan mode sees only read-only tools. The filter runs after
        // the Jev hysteresis filter and the MCP trim so it is the
        // last word — a mode toggle must not be overridable by a
        // per-turn classification.
        let mut definitions = definitions;
        if self.is_in_plan_mode(key).await {
            definitions.retain(|d| {
                matches!(
                    d.name.as_str(),
                    "read_file"
                        | "list_files"
                        | "grep"
                        | "file_info"
                        | "web_search"
                        | "tool_search"
                )
            });
        }
        let grounded = self.ground_prompt(key, prompt, &definitions);
        Ok((alloc, definitions, grounded))
    }

    /// S10: the router classification + Jev memory-filter step that
    /// every `process_*` entry point runs. Extracted so the three
    /// paths cannot drift on this block again.
    ///
    /// `retrieval_log_turn_id` is `Some(id)` when the caller wants
    /// the turn's memory retrieval recorded for `/memory eval`
    /// (`id = 0` from the collected path, `id = trace_id` from the
    /// streaming paths); `None` skips the log entirely (the goal
    /// path's pre-fix behaviour, preserved here).
    pub(crate) async fn classify_and_filter(
        &self,
        key: &str,
        input: &str,
        retrieval_log_turn_id: Option<u64>,
    ) -> Result<crate::router::TaskResponse> {
        let mut response = self.router.process_input(input).await?;
        // P2.5 — drop memory entries Jev judges irrelevant before
        // the prompt budget sees them.
        response.memory_context = self
            .filter_memory_context_with_jev(key, response.memory_context)
            .await;
        // §7.3 — then drop anything injected recently. Order matters:
        // Jev filters by *relevance*, this filters by *novelty*, and
        // an entry that fails both should not have its injection clock
        // started.
        let considered_before_ttl = response
            .memory_context
            .as_ref()
            .map(|c| c.working_memory.len() + c.long_term.len())
            .unwrap_or(0);
        let (ctx, ttl_drops) = self
            .apply_memory_injection_ttl_with_drops(key, response.memory_context)
            .await;
        response.memory_context = ctx;
        // Tier 2.4 — record this turn's retrieval.
        if let Some(turn_id) = retrieval_log_turn_id
            && let Some(ctx) = response.memory_context.as_ref()
        {
            let entries: Vec<(String, f32)> = ctx
                .working_memory
                .iter()
                .chain(ctx.long_term.iter())
                .map(|e| (e.id.to_string(), e.relevance))
                .collect();
            self.log_memory_retrieval(
                turn_id,
                input,
                &entries,
                considered_before_ttl.max(entries.len()),
                &ttl_drops,
            );
        }
        Ok(response)
    }

    /// §7.3: drop memory entries injected within the TTL window.
    ///
    /// Returns the context with recently-shown entries removed and
    /// records the survivors as injected-now. A memory injected 50
    /// minutes ago comes back — by then the turn that consumed it is
    /// far enough back that repeating it is useful, not noise.
    ///
    /// Best-effort: the map is per-transcript, so a swarm agent's view
    /// is its own.
    /// As [`Self::apply_memory_injection_ttl`], but also returns
    /// `(id, reason)` for each entry the TTL dropped, so the retrieval
    /// log can say *why* an entry that scored well was not injected.
    pub(crate) async fn apply_memory_injection_ttl_with_drops(
        &self,
        key: &str,
        context: Option<kod_types::MemoryContext>,
    ) -> (Option<kod_types::MemoryContext>, Vec<(String, String)>) {
        let Some(mut ctx) = context else {
            return (None, Vec::new());
        };
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        let mut guard = self.injected_memory_at.write().await;
        let seen = guard.entry(key.to_string()).or_default();

        let within = |id: &kod_types::MemoryId| -> bool {
            seen.get(id)
                .is_some_and(|&at| now_ms.saturating_sub(at) < MEMORY_INJECTION_TTL_MS)
        };
        let mut drops: Vec<(String, String)> = Vec::new();
        for e in ctx.working_memory.iter().chain(ctx.long_term.iter()) {
            if within(&e.id) {
                let mins = seen
                    .get(&e.id)
                    .map(|&at| now_ms.saturating_sub(at) / 60_000)
                    .unwrap_or(0);
                drops.push((e.id.to_string(), format!("injected {mins}m ago")));
            }
        }
        ctx.working_memory.retain(|e| !within(&e.id));
        ctx.long_term.retain(|e| !within(&e.id));

        for e in ctx.working_memory.iter().chain(ctx.long_term.iter()) {
            seen.insert(e.id.clone(), now_ms);
        }

        // Bound the map: an expired entry cannot suppress anything, so
        // dropping it is exact.
        seen.retain(|_, &mut at| now_ms.saturating_sub(at) < MEMORY_INJECTION_TTL_MS);

        (Some(ctx), drops)
    }

    /// S10: refine the router's task type and skills with Jev's
    /// semantic scoring. Pure data transformation (no I/O except the
    /// Jev calls themselves); the two are always called together.
    pub(crate) async fn refine_classification(
        &self,
        key: &str,
        input: &str,
        response: &crate::router::TaskResponse,
    ) -> (crate::router::TaskType, Vec<String>) {
        let task_type = self
            .refine_task_type_with_jev(key, input, response.task_type)
            .await;
        // P4.2 — augment the router's lexical skill match with
        // Jev's semantic scoring. The union keeps every lexical
        // match and adds semantic ones the substring matcher would
        // have missed. `None` leaves the router's list.
        let refined_skills = self
            .rank_skills_with_jev(key, input, &response.skills_used)
            .await
            .unwrap_or_else(|| response.skills_used.clone());
        (task_type, refined_skills)
    }

    pub async fn process_for(&self, key: &str, input: &str) -> Result<TaskResponse> {
        // Delta §12.3: a due sharpshooter consolidation from the
        // periodic task runs on this turn's engine-side context.
        self.drain_due_sharpshooter().await;
        // H-E5: the non-streaming path never set `current_request`,
        // so the request-keyed Jev helpers (task classification,
        // quality gate) ran against an empty or stale request. The
        // two streaming entry points already do this; the parity
        // matters because the quality gate reads `current_request`
        // *after* `clear_current_request`, which is a separate bug
        // fixed below.
        self.set_current_request(key, input).await;
        // P7: classify the input by its @-references (paths the
        // user mentioned). A turn that mentions .env is sensitive.
        self.update_sensitivity_from_input(input).await;
        // Check if engine is running
        {
            let running = self.is_running.read().await;
            if !*running {
                return Err(KodError::InvalidState("Engine not running".to_string()));
            }
        }
        // Expand `@file` references before the router sees the input.
        // The expanded prompt is what gets classified, what gets
        // remembered, and what reaches the model; the original typed
        // text is only used for the display row the caller already
        // pushed.
        let expanded_input = expand_at_references(input, &self.working_dir);
        let input = expanded_input.as_str();

        // Clone the provider Arc out of the read lock before any long
        // await. Holding the read guard across the agentic loop below
        // made `set_provider` (used by the TUI's `/model` switch) block
        // until the current generation finished — the write acquired
        // only after the last read released, i.e. at the very end of
        // the response. Cloning is one atomic increment on the Arc, so
        // the read lock is held for microseconds.
        let _provider_probe = self.registry.read().await.clone();
        if _provider_probe.is_some() {
            // S10 phase 1: the shared classification + prompt-build
            // pipeline lives in `prepare_turn`. The collected path is
            // the only one that asks the model to plan on the first
            // turn of a Complex/MultiStep task, hence `create_plan =
            // true`. `refined_skills` is consumed once — the old code
            // called `rank_skills_with_jev` twice (once inside
            // `refine_classification`, once inline) with the same
            // inputs, discarding the first result.
            let prep = self.prepare_turn(key, input, Some(0), true).await?;
            let TurnPreparation {
                response,
                task_type,
                refined_skills,
                alloc,
                definitions,
                pending: convo,
                system_text,
                initial_messages,
            } = prep;
            self.snapshot_prompt(key, &convo, &alloc).await;
            // Agentic loop: generate (with tools) -> execute -> feed back.
            // Wrapped in a fallback chain (A6): the primary endpoint is
            // tried first, then each `routing.fallback` endpoint, on
            // errors that `is_retryable()` classifies as transient.
            let options = self.generation_defaults.read().await.to_options();
            let task_key = format!("{:?}", response.task_type);
            // P1: the cache-aware gate needs the fingerprint of the
            // request head and a size estimate for the transcript.
            // Both are cheap: the fingerprint walks the already-
            // rendered system text plus the sorted tool names; the
            // token estimate is a byte-length division, which is
            // what `TokenUsage` uses everywhere it lacks a real
            // tokenizer.
            let head_fingerprint = Self::cache_head_fingerprint(&system_text, &definitions);
            let transcript_tokens: u64 = initial_messages
                .iter()
                .map(|m| (m.content.len() as u64) / 4)
                .sum();
            let mut chain = self
                .resolve_chain_for_task_gated(&task_key, head_fingerprint, transcript_tokens)
                .await;
            if chain.is_empty() {
                return Err(Self::no_provider_error());
            }
            let mut last_err: Option<KodError> = None;
            let mut outcome: Option<(
                String,
                Vec<ToolCall>,
                Vec<ToolResult>,
                Option<kod_provider::TokenUsage>,
            )> = None;
            // The provider that served the winning attempt, kept so the
            // tool-only-reply summary below calls through the same
            // endpoint the model response came from.
            let mut winning_provider: Option<Arc<dyn LlmProvider>> = None;
            let mut winning_model: Option<ModelRef> = None;
            // Delta section 9.7: index-based so the retry-config
            // candidates can be appended to `chain` mid-walk.
            let mut i: usize = 0;
            // M-16: rounds an attempt persists into the shared history
            // must not survive a failed attempt. Captured ONCE here,
            // above the endpoint walk: a fallthrough (`i += 1`) reuses
            // this base, so the next attempt's truncate drops the dead
            // attempt's rounds instead of re-snapshotting the polluted
            // length.
            let attempt_history_base = self.history_len_for(key).await;
            while i < chain.len() {
                let model_ref: ModelRef = chain[i].clone();
                let this_provider = match self
                    .resolve_provider_for_model_ref(&model_ref)
                    .await
                {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::warn!(
                            endpoint = %model_ref.endpoint,
                            error = %e,
                            "cannot resolve endpoint; skipping in chain"
                        );
                        last_err = Some(e);
                        i += 1;
                        continue;
                    }
                };
                let mut attempt_pending = convo.clone();
                let mut attempt_messages = initial_messages.clone();
                // Tier 3.3 — a same-endpoint retry attempt loop. Bounded
                // to 2 retries so a bad strategy cannot chain. Each
                // strategy adjusts the request (temperature, messages,
                // or both) and reruns the same endpoint.
                let mut same_endpoint_attempts: u8 = 0;
                let mut result_opt: Option<
                    Result<(
                        String,
                        Vec<ToolCall>,
                        Vec<ToolResult>,
                        Option<kod_provider::TokenUsage>,
                    )>,
                > = None;
                let mut last_failure: Option<KodError> = None;
                // H-RL1: how many rate-limit windows this call has
                // already slept out. Shared cap with the streaming path.
                let mut rate_limit_retries: u32 = 0;
                loop {
                    // M-16: drop any rounds a prior attempt appended.
                    // `attempt_history_base` is captured once above the
                    // endpoint walk.
                    self.truncate_history_to(key, attempt_history_base).await;
                    let mut attempt_options = options.clone();
                    let round = RoundContext {
                        system_text: &system_text,
                        model_ref: &model_ref,
                        definitions: &definitions,
                        options: &attempt_options,
                        holder: key,
                        trace: None,
                        fallback: None,
                    };
                    let result = self
                        .run_collected_loop(
                            &this_provider,
                            &mut attempt_pending,
                            &mut attempt_messages,
                            &round,
                        )
                        .await;
                    match result {
                        Ok(v) => {
                            result_opt = Some(Ok(v));
                            break;
                        }
                        Err(e) => {
                            // H-RL1: a rate limit (or server-busy
                            // overload, which gets four waits instead of
                            // two) with a waitable hint is slept out
                            // here instead of failing the call. The
                            // collected path has no chunk channel, so
                            // the wait is silent apart from the trace log.
                            let busy = matches!(
                                crate::retry_strategy::TurnFailure::classify(&e.to_string()),
                                crate::retry_strategy::TurnFailure::TransportServerBusy { .. }
                            ) || matches!(e, KodError::ServerBusy { .. });
                            let cap = if busy {
                                MAX_SERVER_BUSY_RETRIES
                            } else {
                                MAX_RATE_LIMIT_RETRIES
                            };
                            if rate_limit_retries < cap
                                && let Some(hint) = self.rate_limit_hint_within_budget(&e)
                            {
                                rate_limit_retries += 1;
                                if busy {
                                    tracing::warn!(
                                        endpoint = %model_ref.endpoint,
                                        hint_secs = hint.as_secs(),
                                        attempt = rate_limit_retries,
                                        "provider overloaded; waiting out the window and re-driving the request"
                                    );
                                } else {
                                    tracing::warn!(
                                        endpoint = %model_ref.endpoint,
                                        hint_secs = hint.as_secs(),
                                        attempt = rate_limit_retries,
                                        "provider rate limit; waiting out the window and re-driving the request"
                                    );
                                }
                                if self.is_cancelled_for(key) {
                                    return Err(KodError::InvalidState(
                                        "cancelled by user".to_string(),
                                    ));
                                }
                                tokio::time::sleep(hint).await;
                                continue;
                            }
                            let failure =
                                crate::retry_strategy::TurnFailure::classify(&e.to_string());
                            let action = crate::retry_strategy::choose_action(&failure);
                            let is_same_endpoint = matches!(
                                action,
                                crate::retry_strategy::RetryAction::SameEndpointLowerTemp
                                    | crate::retry_strategy::RetryAction::SameEndpointConstrained
                                    | crate::retry_strategy::RetryAction::ReinjectTools
                                    | crate::retry_strategy::RetryAction::ShrinkHistory
                            );
                            if is_same_endpoint
                                && same_endpoint_attempts < 2
                                && Self::apply_retry_adjustment(
                                    action,
                                    &mut attempt_options,
                                    &mut attempt_messages,
                                )
                            {
                                same_endpoint_attempts += 1;
                                tracing::warn!(
                                    endpoint = %model_ref.endpoint,
                                    class = %failure.summary(),
                                    action = ?action,
                                    attempt = same_endpoint_attempts,
                                    "retrying same endpoint with adjustment"
                                );
                                continue;
                            }
                            last_failure = Some(e);
                            break;
                        }
                    }
                }
                let collected = match result_opt {
                    Some(Ok(v)) => Ok(v),
                    _ => Err(last_failure.unwrap_or_else(Self::no_provider_error)),
                };
                match collected {
                    Ok(v) => {
                        // Save the pending buffer back: `run_collected_loop`
                        // mutated its own clone, and the summary path below
                        // needs the same mutation (the tool-result block).
                        outcome = Some(v);
                        winning_provider = Some(this_provider);
                        winning_model = Some(model_ref.clone());
                        // We deliberately discard `attempt_pending` here;
                        // the summary path reuses the *original* `convo`.
                        // In practice the summary is short and the model
                        // re-derives from the tool results visible in the
                        // prompt; the pre-A6 behaviour had the same
                        // shape (pending mutated in place, but a
                        // tool-only reply after a fallback is rare
                        // enough that this is acceptable).
                        break;
                    }
                    Err(e) => {
                        // Tier 2.2 — classify the failure and pick a
                        // strategy. Non-recoverable classes surface
                        // immediately; recoverable ones decide whether
                        // to retry the same endpoint (with an
                        // adjustment) or fall through to the next.
                        let failure = crate::retry_strategy::TurnFailure::classify(&e.to_string());
                        let action = crate::retry_strategy::choose_action(&failure);
                        let has_next = i + 1 < chain.len();
                        let should_fall_through = failure.recoverable()
                            && has_next
                            && matches!(
                                action,
                                crate::retry_strategy::RetryAction::NextEndpoint
                                    | crate::retry_strategy::RetryAction::SameEndpointBackoff
                                    | crate::retry_strategy::RetryAction::SameEndpointLowerTemp
                                    | crate::retry_strategy::RetryAction::ShrinkHistory
                                    | crate::retry_strategy::RetryAction::ReinjectTools
                                    | crate::retry_strategy::RetryAction::SameEndpointConstrained
                            );
                        if should_fall_through {
                            let next = &chain[i + 1];
                            tracing::warn!(
                                from = %model_ref.display(),
                                to = %next.display(),
                                error = %e,
                                class = %failure.summary(),
                                action = ?action,
                                "provider error; falling back"
                            );
                            self.record_model_fallback(
                                key,
                                &model_ref,
                                next,
                                &format!("{} ({})", e, failure.summary()),
                            )
                            .await;
                            // Hygiene 3.2: record the failure against
                            // the endpoint that produced it.
                            if let Ok(mut h) = self.endpoint_health.lock()
                                && h.record_failure(&model_ref.endpoint, e.to_string())
                            {
                                tracing::warn!(
                                    endpoint = %model_ref.endpoint,
                                    "circuit breaker tripped; skipping for cooldown",
                                );
                            }
                            last_err = Some(e);
                            i += 1;
                            continue;
                        }
                        // Delta section 9.7: the static chain is
                        // exhausted. Consult the per-class retry
                        // config for additional candidates; append
                        // them and keep walking if it supplies any.
                        if failure.recoverable()
                            && matches!(
                                action,
                                crate::retry_strategy::RetryAction::NextEndpoint
                                    | crate::retry_strategy::RetryAction::SameEndpointBackoff
                                    | crate::retry_strategy::RetryAction::SameEndpointLowerTemp
                                    | crate::retry_strategy::RetryAction::ShrinkHistory
                                    | crate::retry_strategy::RetryAction::ReinjectTools
                                    | crate::retry_strategy::RetryAction::SameEndpointConstrained
                            )
                        {
                            let extra = self
                                .retry_chain_candidates(
                                    &model_ref.display(),
                                    failure.class_name(),
                                    &chain,
                                )
                                .await;
                            if !extra.is_empty() {
                                tracing::warn!(
                                    failed = %model_ref.display(),
                                    class = %failure.class_name(),
                                    added = extra.len(),
                                    "retry chain: appending per-class candidates",
                                );
                                chain.extend(extra);
                                i += 1;
                                continue;
                            }
                        }
                        return Err(e);
                    }
                }
            }
            let (final_text, tool_calls, tool_results, usage) =
                outcome.ok_or_else(|| last_err.unwrap_or_else(Self::no_provider_error))?;
            // The winning endpoint's configured pricing. `None` for
            // a local endpoint (no cost), a remote endpoint without
            // a `[pricing]` block, or a response that never reached
            // the provider.
            let pricing = match &winning_model {
                Some(m) => self.pricing_for(m).await,
                None => None,
            };
            // Persist the cost line for this call, when we know the
            // pricing. One line per call, not per session — a session
            // log read later can reconstruct the total by summing,
            // and a per-turn figure is what a debug pass needs.
            // Delta §2.4: usage must be recorded regardless of
            // whether pricing is known. The context gauge (anchored
            // context-token estimate) and the cache ledger both
            // depend on the settled `usage`, not on cost. The
            // session-log cost line is the only thing that needs
            // `pricing`, so it lives inside the optional binding
            // in `record_cost_with_head`.
            if let (Some(m), Some(u)) = (winning_model.as_ref(), usage.as_ref()) {
                // P1: pass the fingerprint of the request head this
                // call actually served so the ledger knows which
                // endpoint is warm for which prefix.
                let head_fp = Self::cache_head_fingerprint(&system_text, &definitions);
                self.record_cost_with_head(key, m, u, pricing, head_fp)
                    .await;
                // Hygiene 3.2: a successful call clears the breaker.
                if let Ok(mut h) = self.endpoint_health.lock() {
                    h.record_success(&m.endpoint);
                }
            }
            // Model only called tools and never wrote back: ask for a summary.
            let final_text = if final_text.trim().is_empty() && !tool_calls.is_empty() {
                let mut summary_prompt = convo.clone();
                // Serialize the round's tool results into the prompt:
                // the summary call goes through the legacy
                // `generate(&str)` API, so the model has to see them
                // as text here. Regression guard: an earlier shape
                // sent only `convo`, which is the *pre-loop* prompt,
                // so the model was asked to summarize work it could
                // not see.
                if !tool_results.is_empty() {
                    summary_prompt.push_str("\n\n## Tool results from this turn\n");
                    // Harness review hygiene: render each result
                    // through `summarize_tool_result` (the same
                    // structured text the TUI shows for a tool row),
                    // not Rust's `Debug` impl. A `{:?}` embeds the
                    // enum's internal shape and escape sequences,
                    // wasting prompt tokens on a form the model was
                    // never trained on.
                    let calls: Vec<_> = tool_calls.iter().collect();
                    for (i, r) in tool_results.iter().enumerate() {
                        let name = calls.get(i).map(|c| c.tool_name.as_str()).unwrap_or("tool");
                        summary_prompt.push_str(&format!(
                            "\n### Result {}\n{}\n",
                            i + 1,
                            summarize_tool_result(name, r),
                        ));
                    }
                }
                summary_prompt.push_str(
                    "\nSummarize what you did and the result for the user in plain text.",
                );
                let summary_provider = winning_provider.ok_or_else(Self::no_provider_error)?;
                summary_provider.generate(&summary_prompt, &options).await?
            } else {
                final_text
            };

            // Research mode: verify file:line citations before
            // returning. Purely local — no extra LLM call, no
            // network. The block appears only when at least one
            // citation fails to verify; a clean reply stays clean.
            let final_text = if matches!(task_type, crate::router::TaskType::Research) {
                let syntactic =
                    crate::citations::check_and_annotate(&final_text, &self.working_dir).text;
                // P4.5 — after the syntactic check, run the semantic
                // pass. The two are additive: the syntactic block
                // reports missing/out-of-range citations, the
                // semantic block reports lines that do not support
                // their claim.
                self.semantic_verify_citations(key, &syntactic).await
            } else {
                final_text
            };

            // M-14: read the request BEFORE clearing it. The gate and
            // the stop classifier both need it; the old order fed them
            // "" on the collected path.
            let request_text = self.current_request(key).await.unwrap_or_default();
            self.clear_current_request(key).await;

            // P5.4 — response quality gate. Non-blocking: the
            // reply is delivered unchanged; only an advisory is
            // appended when Jev is confident the reply missed
            // the request.
            let final_text = match self
                .check_response_quality_with_jev(key, &request_text, &final_text)
                .await
            {
                Some(advisory) => format!("{final_text}{advisory}"),
                None => final_text,
            };
            // Delta §7.7 item 5: a clean-stop turn (no tool calls)
            // may have emitted an OpenAI-style patch as prose. Lift it
            // and run it as synthetic `patch_file` calls through the
            // normal approval path.
            if tool_calls.is_empty()
                && let Some(note) =
                    self.maybe_recover_inline_patch(key, &final_text, None).await
            {
                tracing::info!(holder = %key, note = %note, "recovered inline patch");
            }
            self.remember_turn_for(key, false, &final_text).await;
            // Delta §11.7: remind the model of open todos at stop.
            self.maybe_emit_todo_completion_reminder(key, &final_text)
                .await;
            // Delta §12.6: continuous memory extraction over the new
            // tail since the last pass.
            self.maybe_extract_continuously(key).await;
            // Delta §12.3: friction-gated decision extraction from
            // the user prompt for this turn. Best-effort; every
            // failure logs and returns.
            self.maybe_extract_decisions(key).await;

            // Delta §9.4 (diagnostic): same classifier call as the
            // streaming path. Non-blocking.
            self.diagnose_unexpected_stop(key, &request_text, &final_text, tool_calls.len())
                .await;

            return Ok(TaskResponse {
                task_type: response.task_type,
                text: Some(final_text),
                tool_calls,
                tool_results,
                skills_used: refined_skills.clone(),
                memory_used: response.memory_used,
                execution_time_ms: response.execution_time_ms,
                usage,
                pricing,
                memory_context: response.memory_context,
            });
        }

        // No provider. Reject rather than return the router's
        // placeholder text — see `no_provider_error`.
        Err(Self::no_provider_error())
    }

    /// Process user input, streaming text chunks live to `chunk_tx`.
    ///
    /// Same result as [`process`], but answer tokens arrive as they generate
    /// (the TUI renders each chunk immediately) and tool starts arrive as
    /// [`tool_start_marker`] chunks (see [`parse_tool_start`]) so the UI can
    /// show "running …" while the tool actually executes.
    pub async fn process_streaming(
        &self,
        input: &str,
        chunk_tx: &tokio::sync::mpsc::Sender<String>,
    ) -> Result<TaskResponse> {
        self.process_streaming_for(DEFAULT_TRANSCRIPT_KEY, input, chunk_tx)
            .await
    }

    /// Streaming variant of [`KodEngine::process_for`].
    pub async fn process_streaming_for(
        &self,
        key: &str,
        input: &str,
        chunk_tx: &tokio::sync::mpsc::Sender<String>,
    ) -> Result<TaskResponse> {
        self.process_streaming_with_model_for(key, input, chunk_tx, None, None)
            .await
    }

    /// Streaming variant of [`KodEngine::process_for`] with an optional
    /// model override.
    ///
    /// The override is the swarm runner's per-capability routing hook
    /// (design D1.4 PR A7): a subtask's `capability` (Coding, Testing,
    /// CodeReview, …) resolves against `[llm.routing.swarm]` to a
    /// `ModelRef`, and the streaming chain tries that model first.
    /// Fallback endpoints from `[llm.routing].fallback` are still
    /// appended — the override replaces the *primary* choice, not the
    /// fallback chain.
    ///
    /// `None` routes by task type as before; this is what every
    /// non-swarm caller passes, and it is what
    /// `process_streaming_for` itself passes.
    pub async fn process_streaming_with_model_for(
        &self,
        key: &str,
        input: &str,
        chunk_tx: &tokio::sync::mpsc::Sender<String>,
        override_model: Option<ModelRef>,
        effort: Option<kod_types::effort::EffortLevel>,
    ) -> Result<TaskResponse> {
        {
            let running = self.is_running.read().await;
            if !*running {
                return Err(KodError::InvalidState("Engine not running".to_string()));
            }
        }
        self.set_current_request(key, input).await;
        // Delta §13.2: fold this user message's behavioral signals
        // into the session total.
        {
            let s = kod_stats::behavioral::analyze(input);
            let mut b = self.behavioral.lock();
            *b = b.merge(&s);
        }
        // Delta §11.7: offer the todo tool at the start of a task.
        self.maybe_offer_todo_prelude(key, input).await;
        // Tier 1.1 — a fresh user turn clears any prior taint.
        self.reset_taint();
        // Delta §11.8: a fresh user turn resets the advisor
        // emission budget. The dedupe history persists — a note
        // admitted last turn is still a duplicate this turn.
        self.advisor_guard.lock().begin_update();
        // Delta §14.3: advance the TTSR turn counter. A `Gap(n)` rule
        // refires only when n turns have passed since its last fire,
        // so this is what makes the gap semantics live.
        self.begin_ttsr_turn().await;
        self.tool_counts.begin_turn();
        // Tier 1.4 — open a turn trace. Emitted when this call returns.
        let trace_id = self.next_turn_id();
        let mut trace_builder = crate::trace::TurnTraceBuilder::new(trace_id, key);
        trace_builder.set_user_prompt(input);
        let trace = std::sync::Mutex::new(trace_builder);
        let trace_ref: Option<&std::sync::Mutex<crate::trace::TurnTraceBuilder>> = Some(&trace);
        self.cost_tracker.begin_turn();
        // P3.3 — ask Jev whether the request is ambiguous; if so and
        // a streaming consumer is attached, prompt for clarification
        // before the LLM ever sees the request.
        let clarified = self
            .augment_input_with_jev_ambiguity_check(key, input, Some(chunk_tx))
            .await;
        if clarified != input {
            // Update the stored request so downstream Jev checks see
            // the clarified version.
            self.set_current_request(key, &clarified).await;
        }
        let expanded_input = expand_at_references(&clarified, &self.working_dir);
        let input = expanded_input.as_str();

        // See process(): clone out of the lock before any long await.
        let _provider_probe = self.registry.read().await.clone();
        if _provider_probe.is_some() {
            // S10 phase 2: same pipeline as `process_for`, but the
            // streaming path uses the real trace id for the retrieval
            // log (so `/memory eval` correlates entries with the reply
            // that used them) and does not ask the model to plan on
            // the first turn — the streaming loop relies on the model
            // reaching for tools itself. The goal path is the third
            // caller and the last to migrate.
            let prep = self.prepare_turn(key, input, Some(trace_id), false).await?;
            let TurnPreparation {
                response,
                task_type,
                refined_skills,
                alloc,
                definitions,
                pending,
                system_text,
                initial_messages,
            } = prep;
            self.snapshot_prompt(key, &pending, &alloc).await;
            let mut options = self.generation_defaults.read().await.to_options();
            // Delta §9.6: resolve the turn's reasoning effort.
            // Precedence is caller > endpoint config (fixed) >
            // endpoint config ("auto" → judge classification) >
            // generation default. The resolver's own doc names the
            // silent-fallthrough contract when no judge role is
            // configured.
            {
                let primary = self.current_model.read().await.clone();
                if let Some(level) = self.resolve_turn_effort(input, &primary, effort).await {
                    options.effort = Some(level);
                }
            }
            // Fallback chain (A6). Streaming retries reuse the same
            // chunk_tx, so a successful fallback continues the visible
            // stream exactly where the failed attempt stopped; a
            // retryable error typically fires before any token, so the
            // user sees a clean stream from the fallback endpoint.
            let task_key = format!("{:?}", response.task_type);
            let mut chain = self
                .build_streaming_chain(&task_key, override_model.as_ref())
                .await;
            if chain.is_empty() {
                return Err(Self::no_provider_error());
            }
            let mut last_err: Option<KodError> = None;
            let mut outcome: Option<(
                String,
                Vec<ToolCall>,
                Vec<ToolResult>,
                Option<kod_provider::TokenUsage>,
            )> = None;
            let mut winning_provider: Option<Arc<dyn LlmProvider>> = None;
            let mut winning_model: Option<ModelRef> = None;
            // Delta section 9.7: index-based so per-class retry-chain
            // candidates can be appended to `chain` mid-walk.
            let mut i: usize = 0;
            // M-16: same as the collected path — captured once above
            // the endpoint walk so a fallthrough reuses the pre-chain
            // base.
            let attempt_history_base = self.history_len_for(key).await;
            while i < chain.len() {
                let model_ref: ModelRef = chain[i].clone();
                let this_provider = match self
                    .resolve_provider_for_model_ref(&model_ref)
                    .await
                {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::warn!(
                            endpoint = %model_ref.endpoint,
                            error = %e,
                            "cannot resolve endpoint; skipping in chain"
                        );
                        last_err = Some(e);
                        // Advance the cursor: `continue` alone re-tested
                        // the same index and spun forever.
                        i += 1;
                        continue;
                    }
                };
                let mut attempt_pending = pending.clone();
                let mut attempt_messages = initial_messages.clone();
                // P5.6 — the next endpoint in the chain, if any, is
                // the mid-stream switch target for `stream_round`.
                // `None` on the last chain entry, which preserves the
                // pre-P5.6 behaviour there.
                let fallback_ref = chain.get(i + 1);
                let round = RoundContext {
                    system_text: &system_text,
                    model_ref: &model_ref,
                    definitions: &definitions,
                    options: &options,
                    holder: key,
                    trace: trace_ref,
                    fallback: fallback_ref,
                };
                // H-RL1: a rate-limited turn is slept out (up to the
                // configured budget, MAX_RATE_LIMIT_RETRIES times —
                // server-busy overload gets MAX_SERVER_BUSY_RETRIES)
                // and re-driven on the SAME endpoint instead of
                // surfacing the error and forcing a manual retry.
                // Cancellation during the wait surfaces as the normal
                // cancel error.
                let mut rate_limit_attempt: u32 = 0;
                let loop_outcome = loop {
                    // M-16: drop any rounds a prior attempt appended.
                    // `attempt_history_base` is captured once above the
                    // endpoint walk.
                    self.truncate_history_to(key, attempt_history_base).await;
                    match self
                        .run_streaming_loop(
                            &this_provider,
                            &mut attempt_pending,
                            &mut attempt_messages,
                            chunk_tx,
                            &round,
                        )
                        .await
                    {
                        Ok(v) => break Ok(v),
                        Err(e) => match self
                            .rate_limit_retry_wait(&e, rate_limit_attempt, key, chunk_tx)
                            .await
                        {
                            Ok(Some(())) => {
                                rate_limit_attempt += 1;
                                continue;
                            }
                            Ok(None) => break Err(e),
                            Err(cancelled) => break Err(cancelled),
                        },
                    }
                };
                match loop_outcome {
                    Ok((text, calls, results, usage, retry)) => {
                        // P5.6 — Jev said the round was off-track. If
                        // a fallback endpoint remains, keep the
                        // retry going; otherwise accept the result
                        // (fail-open: a bad reply is better than no
                        // reply).
                        if retry && i + 1 < chain.len() {
                            let next = &chain[i + 1];
                            tracing::warn!(
                                from = %model_ref.display(),
                                to = %next.display(),
                                "Jev flagged the reply off-track; trying next endpoint"
                            );
                            self.record_model_fallback(
                                key,
                                &model_ref,
                                next,
                                "jev quality gate",
                            )
                            .await;
                            last_err = None;
                            i += 1;
                            continue;
                        }
                        outcome = Some((text, calls, results, usage));
                        winning_provider = Some(this_provider);
                        winning_model = Some(model_ref.clone());
                        break;
                    }
                    Err(e) if e.is_retryable() && i + 1 < chain.len() => {
                        let next = &chain[i + 1];
                        // A retryable error *usually* fires before any
                        // token, but not always — a stream can produce
                        // output and then fail. Without a reset the
                        // fallback's text appends to the dead
                        // attempt's, and the user reads a sentence
                        // that was never said by one model. Emitting
                        // the reset is unconditional: on the common
                        // path it drops an empty bubble, a no-op.
                        let _ = chunk_tx.send(stream_reset_marker()).await;
                        tracing::warn!(
                            from = %model_ref.display(),
                            to = %next.display(),
                            error = %e,
                            "retryable provider error; falling back"
                        );
                        self.record_model_fallback(
                            key,
                            &model_ref,
                            next,
                            &e.to_string(),
                        )
                        .await;
                        // Hygiene 3.2: record the endpoint failure.
                        if let Ok(mut h) = self.endpoint_health.lock()
                            && h.record_failure(&model_ref.endpoint, e.to_string())
                        {
                            tracing::warn!(
                                endpoint = %model_ref.endpoint,
                                "circuit breaker tripped; skipping for cooldown",
                            );
                        }
                        last_err = Some(e);
                        i += 1;
                        continue;
                    }
                    Err(e) => {
                        // Delta section 9.7: the static chain has no
                        // next entry. Consult the per-class retry
                        // config for additional candidates; append
                        // and keep walking if it supplies any.
                        let failure =
                            crate::retry_strategy::TurnFailure::classify(&e.to_string());
                        let action = crate::retry_strategy::choose_action(&failure);
                        if failure.recoverable()
                            && matches!(
                                action,
                                crate::retry_strategy::RetryAction::NextEndpoint
                                    | crate::retry_strategy::RetryAction::SameEndpointBackoff
                                    | crate::retry_strategy::RetryAction::SameEndpointLowerTemp
                                    | crate::retry_strategy::RetryAction::ShrinkHistory
                                    | crate::retry_strategy::RetryAction::ReinjectTools
                                    | crate::retry_strategy::RetryAction::SameEndpointConstrained
                            )
                        {
                            let extra = self
                                .retry_chain_candidates(
                                    &model_ref.display(),
                                    failure.class_name(),
                                    &chain,
                                )
                                .await;
                            if !extra.is_empty() {
                                // Reset the stream so the fallback's
                                // text does not append to this
                                // attempt's partial output.
                                let _ = chunk_tx.send(stream_reset_marker()).await;
                                tracing::warn!(
                                    failed = %model_ref.display(),
                                    class = %failure.class_name(),
                                    added = extra.len(),
                                    "retry chain: appending per-class candidates",
                                );
                                if let Ok(mut h) = self.endpoint_health.lock() {
                                    h.record_failure(&model_ref.endpoint, e.to_string());
                                }
                                chain.extend(extra);
                                last_err = Some(e);
                                i += 1;
                                continue;
                            }
                        }
                        return Err(e);
                    }
                }
            }
            let (final_text, tool_calls, tool_results, usage) =
                outcome.ok_or_else(|| last_err.unwrap_or_else(Self::no_provider_error))?;
            // The winning endpoint's configured pricing. `None` for
            // a local endpoint (no cost), a remote endpoint without
            // a `[pricing]` block, or a response that never reached
            // the provider.
            let pricing = match &winning_model {
                Some(m) => self.pricing_for(m).await,
                None => None,
            };
            // Persist the cost line for this call, when we know the
            // pricing. One line per call, not per session — a session
            // log read later can reconstruct the total by summing,
            // and a per-turn figure is what a debug pass needs.
            // Delta §2.4: usage must be recorded regardless of
            // whether pricing is known. The context gauge (anchored
            // context-token estimate) and the cache ledger both
            // depend on the settled `usage`, not on cost. The
            // session-log cost line is the only thing that needs
            // `pricing`, so it lives inside the optional binding
            // in `record_cost_with_head`.
            if let (Some(m), Some(u)) = (winning_model.as_ref(), usage.as_ref()) {
                // P1: pass the fingerprint of the request head this
                // call actually served so the ledger knows which
                // endpoint is warm for which prefix.
                let head_fp = Self::cache_head_fingerprint(&system_text, &definitions);
                self.record_cost_with_head(key, m, u, pricing, head_fp)
                    .await;
                // Hygiene 3.2: a successful call clears the breaker.
                if let Ok(mut h) = self.endpoint_health.lock() {
                    h.record_success(&m.endpoint);
                }
            }
            let final_text = if final_text.trim().is_empty() && !tool_calls.is_empty() {
                let mut summary_prompt = pending.clone();
                // Same treatment as the collected path: the summary
                // call cannot see the structured messages, so the
                // results are rendered into the text prompt.
                if !tool_results.is_empty() {
                    summary_prompt.push_str("\n\n## Tool results from this turn\n");
                    // Harness review hygiene: render each result
                    // through `summarize_tool_result` (the same
                    // structured text the TUI shows for a tool row),
                    // not Rust's `Debug` impl. A `{:?}` embeds the
                    // enum's internal shape and escape sequences,
                    // wasting prompt tokens on a form the model was
                    // never trained on.
                    let calls: Vec<_> = tool_calls.iter().collect();
                    for (i, r) in tool_results.iter().enumerate() {
                        let name = calls.get(i).map(|c| c.tool_name.as_str()).unwrap_or("tool");
                        summary_prompt.push_str(&format!(
                            "\n### Result {}\n{}\n",
                            i + 1,
                            summarize_tool_result(name, r),
                        ));
                    }
                }
                summary_prompt.push_str(
                    "\nSummarize what you did and the result for the user in plain text.",
                );
                let summary_provider = winning_provider.ok_or_else(Self::no_provider_error)?;
                self.stream_summary(&summary_provider, &summary_prompt, &options, chunk_tx)
                    .await?
            } else {
                final_text
            };

            // Research mode: verify file:line citations. Same pass
            // as the collected path, plus a chunk over the stream so
            // the TUI shows the block as part of the reply, not as
            // a separate message. A clean reply emits nothing.
            let final_text = if matches!(task_type, crate::router::TaskType::Research) {
                let annotated =
                    crate::citations::check_and_annotate(&final_text, &self.working_dir);
                if let Some(block) = &annotated.block {
                    let _ = chunk_tx.send(format!("\n\n{block}")).await;
                }
                annotated.text
            } else {
                final_text
            };

            // P5.4 — response quality gate. Non-blocking: the
            // reply is delivered unchanged; only an advisory is
            // appended when Jev is confident the reply missed
            // the request. A Jev round-trip stalls the visible turn,
            // so announce it instead of a stale "thinking…".
            let _ = chunk_tx.send(activity_marker("reviewing reply…")).await;
            let request_text = self.current_request(key).await.unwrap_or_default();
            let final_text = match self
                .check_response_quality_with_jev(key, &request_text, &final_text)
                .await
            {
                Some(advisory) => format!("{final_text}{advisory}"),
                None => final_text,
            };
            // Delta §7.7 item 5: a clean-stop turn (no tool calls)
            // may have emitted an OpenAI-style patch as prose. Lift it
            // and run it as synthetic `patch_file` calls through the
            // normal approval path. Any tool rows the recovery
            // produces stream back on `chunk_tx`.
            if tool_calls.is_empty()
                && let Some(note) = self
                    .maybe_recover_inline_patch(key, &final_text, Some(chunk_tx))
                    .await
            {
                tracing::info!(holder = %key, note = %note, "recovered inline patch");
                let _ = chunk_tx
                    .send(format!("

{note}
"))
                    .await;
            }
            self.remember_turn_for(key, false, &final_text).await;
            // Delta §11.7: remind the model of open todos at stop.
            self.maybe_emit_todo_completion_reminder(key, &final_text)
                .await;
            // Delta §12.6: continuous memory extraction over the new
            // tail since the last pass — fact creation, not thinking.
            let _ = chunk_tx.send(activity_marker("saving memories…")).await;
            self.maybe_extract_continuously(key).await;
            // Delta §12.3: friction-gated decision extraction from
            // the user prompt for this turn. Best-effort; every
            // failure logs and returns.
            // Tier 3.4 (below): durable decisions — two more Jev
            // calls. One marker covers both decision passes.
            let _ = chunk_tx
                .send(activity_marker("extracting decisions…"))
                .await;
            self.maybe_extract_decisions(key).await;

            // Delta §9.4 (diagnostic): classify a clean stop with
            // no tool calls. Non-blocking — the reply is delivered
            // unchanged; only a log line records the verdict.
            // Wired here after `remember_turn_for` so the classifier
            // sees the same `final_text` the caller will receive.
            let _ = chunk_tx.send(activity_marker("reviewing turn…")).await;
            self.diagnose_unexpected_stop(key, &request_text, &final_text, tool_calls.len())
                .await;

            // Tier 3.4 — extract durable decisions from this turn.
            // Two Jev calls, gated; no-op when Jev is disabled.
            // (Announced by the "extracting decisions…" marker above.)
            let _ = self
                .extract_decisions_with_jev(key, trace_id, input, &final_text)
                .await;

            // Tier 1.4 — finish and emit the trace.
            if let Ok(mut g) = trace.lock() {
                g.set_reply_chars(final_text.len());
            }
            // We have to move the builder out of the Mutex to finish
            // it; since we are the only owner at this point, this is
            // a simple `into_inner` on the tracked cell.
            let finished = match std::sync::Arc::try_unwrap(std::sync::Arc::new(trace)) {
                Ok(m) => m.into_inner().unwrap_or_else(|e| e.into_inner()),
                Err(_) => {
                    return Ok(TaskResponse {
                        task_type: response.task_type,
                        text: Some(final_text),
                        tool_calls,
                        tool_results,
                        skills_used: refined_skills.clone(),
                        memory_used: response.memory_used,
                        execution_time_ms: response.execution_time_ms,
                        usage,
                        pricing,
                        memory_context: response.memory_context,
                    });
                }
            };
            self.emit_turn_trace(&finished.finish());

            return Ok(TaskResponse {
                task_type: response.task_type,
                text: Some(final_text),
                tool_calls,
                tool_results,
                skills_used: refined_skills.clone(),
                memory_used: response.memory_used,
                execution_time_ms: response.execution_time_ms,
                usage,
                pricing,
                memory_context: response.memory_context,
            });
        }

        Err(Self::no_provider_error())
    }

    /// Work toward `goal` across turns until the model declares it met.
    ///
    /// Same streaming contract as [`process_streaming`], but after each
    /// agentic pass the conversation continues with a "keep going" nudge
    /// until the reply contains `GOAL MET` (case-insensitive),
    /// [`MAX_GOAL_TURNS`] passes run, or [`KodEngine::request_cancel`]
    /// fires. Steer notes queued via [`KodEngine::steer`] are injected
    /// every turn. Each turn's text streams live; turns are separated by
    /// a turn-marker chunk (see [`turn_marker`]) so the TUI can render progress.
    pub async fn process_goal_streaming(
        &self,
        input: &str,
        goal: &str,
        chunk_tx: &tokio::sync::mpsc::Sender<String>,
    ) -> Result<TaskResponse> {
        self.process_goal_streaming_for(DEFAULT_TRANSCRIPT_KEY, input, goal, chunk_tx)
            .await
    }

    /// Streaming goal loop on a named transcript.
    pub async fn process_goal_streaming_for(
        &self,
        key: &str,
        input: &str,
        goal: &str,
        chunk_tx: &tokio::sync::mpsc::Sender<String>,
    ) -> Result<TaskResponse> {
        {
            let running = self.is_running.read().await;
            if !*running {
                return Err(KodError::InvalidState("Engine not running".to_string()));
            }
        }
        self.set_current_request(key, input).await;
        // P3.3 — ask Jev whether the request is ambiguous; if so and
        // a streaming consumer is attached, prompt for clarification
        // before the LLM ever sees the request.
        let clarified = self
            .augment_input_with_jev_ambiguity_check(key, input, Some(chunk_tx))
            .await;
        if clarified != input {
            // Update the stored request so downstream Jev checks see
            // the clarified version.
            self.set_current_request(key, &clarified).await;
        }
        let expanded_input = expand_at_references(&clarified, &self.working_dir);
        let input = expanded_input.as_str();

        // See process(): clone out of the lock before any long await.
        let _provider_probe = self.registry.read().await.clone();
        if _provider_probe.is_some() {
            // Delta §11.6: start the goal in the runtime. A second
            // call replaces any previous goal (status Dropped).
            self.goal_runtime.write().await.start(goal.to_string());
            // S10 phase 3: same shared pipeline as the other two entry
            // points. The goal path is the one caller that does *not*
            // write a retrieval-log entry (`None`), and the one that
            // augments both `pending` and `system_text` with a `## Goal`
            // block *after* the shared pipeline returns. `snapshot_prompt`
            // therefore runs *after* the augmentation, so `/debug
            // last-prompt` shows the goal the model actually saw.
            let prep = self.prepare_turn(key, input, None, false).await?;
            let TurnPreparation {
                response,
                // The goal path does not re-consult the task type
                // after `prepare_turn` returns — the goal block is the
                // steering signal. `refined_skills` is still consumed
                // in the final `TaskResponse`, so it stays bound.
                task_type: _,
                refined_skills,
                alloc,
                definitions,
                mut pending,
                system_text,
                initial_messages: _,
            } = prep;
            let goal_block = format!(
                "\n## Goal\n\n{goal}\n\nWork turn by turn toward this goal using tools. Do not ask the user for confirmation — act. When the goal is fully reached, end your reply with a line containing exactly GOAL MET and summarize what was done. If a tool errors, work around it and keep going.\n"
            );
            pending.push_str(&goal_block);
            self.snapshot_prompt(key, &pending, &alloc).await;
            // Structured inputs for the streaming loop. The goal text
            // is part of the system prompt, not a message, so a fresh
            // turn of the goal loop does not create a duplicate user
            // message every iteration.
            let system_text = {
                let mut s = system_text;
                s.push_str(&goal_block);
                s
            };
            // The user message for the initial turn. The goal loop
            // re-uses the same structured base across iterations; the
            // `pending` text grows per turn, but the message list is
            // rebuilt from the transcript + this user turn.
            // The goal loop runs turn by turn; each turn's request
            // must include everything the previous turns produced
            // (assistant text, tool calls, tool results). Pre-migration
            // the text `pending` was mutated in place across turns, so
            // turn 2 saw turn 1; the structured path needs the same
            // accumulation to preserve that behaviour.
            let mut goal_messages: Vec<kod_types::ChatMessage> = {
                let guard = self.history.read().await;
                guard.get(key).cloned().unwrap_or_default()
            };

            let options = self.generation_defaults.read().await.to_options();
            // Resolve the fallback chain once; reused across turns.
            let task_key = format!("{:?}", response.task_type);
            let goal_chain = self.resolve_chain_for_task(&task_key).await;
            if goal_chain.is_empty() {
                return Err(Self::no_provider_error());
            }
            let mut all_text = String::new();
            let mut tool_calls: Vec<ToolCall> = Vec::new();
            let mut tool_results: Vec<ToolResult> = Vec::new();
            let mut last_usage: Option<kod_provider::TokenUsage> = None;
            for turn in 1..=MAX_GOAL_TURNS {
                // H-E8: cancel on the transcript's own key, not the
                // default. A swarm agent's `request_cancel_for`
                // ("swarm:<id>") was never observed here; only the
                // default transcript's cancel flag was checked, so a
                // swarm cancel left the goal loop running.
                if self.is_cancelled_for(key) {
                    return Err(KodError::InvalidState("cancelled by user".to_string()));
                }
                // Delta §9.8: the pause gate's model-call boundary.
                self.pause_gate.wait_if_paused().await;
                if turn > 1 {
                    let _ = chunk_tx.send(turn_marker(turn as u32)).await;
                    let nudge = "Continue working toward the goal above. If it is now fully reached, reply with GOAL MET plus a short summary instead of calling more tools.";
                    pending.push_str(&format!("\n\n{nudge}\n"));
                    // The nudge is what tells the model to *continue*;
                    // on the structured path it has to be a message
                    // for the provider to see it.
                    goal_messages.push(kod_types::ChatMessage::text(
                        kod_types::MessageId::new(),
                        kod_types::MessageRole::User,
                        nudge.to_string(),
                        time::OffsetDateTime::now_utc(),
                    ));
                }
                self.apply_steers(&mut pending, &mut goal_messages, key)
                    .await;
                // Per-turn fallback chain (A6). The chain is resolved
                // once outside the turn loop and reused, so a fallback
                // chosen on turn N is also the primary for turn N+1.
                let mut turn_outcome: Option<(
                    String,
                    Vec<ToolCall>,
                    Vec<ToolResult>,
                    Option<kod_provider::TokenUsage>,
                    // P5.6 — ignored in the goal loop; only the
                    // streaming single-turn path uses the retry
                    // signal.
                    bool,
                )> = None;
                let mut turn_err: Option<KodError> = None;
                for (i, model_ref) in goal_chain.iter().enumerate() {
                    let this_provider = match self.resolve_provider_for_model_ref(model_ref).await {
                        Ok(p) => p,
                        Err(e) => {
                            tracing::warn!(
                                endpoint = %model_ref.endpoint,
                                error = %e,
                                "cannot resolve endpoint; skipping in chain"
                            );
                            turn_err = Some(e);
                            continue;
                        }
                    };
                    let mut attempt_pending = pending.clone();
                    let mut attempt_messages = goal_messages.clone();
                    let round = RoundContext {
                        system_text: &system_text,
                        model_ref,
                        definitions: &definitions,
                        options: &options,
                        holder: key,
                        trace: None,
                        fallback: None,
                    };
                    // H-RL1: same contract as the streaming chain — a
                    // rate-limited goal turn is slept out and re-driven
                    // on the same endpoint.
                    let mut rate_limit_attempt: u32 = 0;
                    let turn_outcome_result = loop {
                        match self
                            .run_streaming_loop(
                                &this_provider,
                                &mut attempt_pending,
                                &mut attempt_messages,
                                chunk_tx,
                                &round,
                            )
                            .await
                        {
                            Ok(v) => break Ok(v),
                            Err(e) => match self
                                .rate_limit_retry_wait(&e, rate_limit_attempt, key, chunk_tx)
                                .await
                            {
                                Ok(Some(())) => {
                                    rate_limit_attempt += 1;
                                    continue;
                                }
                                Ok(None) => break Err(e),
                                Err(cancelled) => break Err(cancelled),
                            },
                        }
                    };
                    match turn_outcome_result {
                        Ok(v) => {
                            // Fold this turn's extended messages back
                            // so the next turn starts from the full
                            // accumulated conversation, not the
                            // pre-loop snapshot.
                            goal_messages = attempt_messages;
                            turn_outcome = Some(v);
                            break;
                        }
                        Err(e) if e.is_retryable() && i + 1 < goal_chain.len() => {
                            let next = &goal_chain[i + 1];
                            tracing::warn!(
                                from = %model_ref.display(),
                                to = %next.display(),
                                error = %e,
                                "retryable provider error; falling back"
                            );
                            self.record_model_fallback(key, model_ref, next, &e.to_string())
                                .await;
                            turn_err = Some(e);
                            continue;
                        }
                        Err(e) => return Err(e),
                    }
                }
                let (final_text, calls, results, usage, _retry) =
                    turn_outcome.ok_or_else(|| turn_err.unwrap_or_else(Self::no_provider_error))?;
                // Delta §11.6: account this turn's usage against the
                // active goal before merging into the session total.
                if let Some(u) = usage.as_ref() {
                    self.goal_runtime.write().await.observe_usage(u);
                }
                last_usage = match (last_usage, usage) {
                    (Some(prev), Some(next)) => Some(prev.merge(&next)),
                    (Some(prev), None) => Some(prev),
                    (None, Some(next)) => Some(next),
                    (None, None) => None,
                };
                if !all_text.is_empty() && !final_text.trim().is_empty() {
                    all_text.push_str("\n\n");
                }
                all_text.push_str(&final_text);
                tool_calls.extend(calls);
                tool_results.extend(results);
                if reply_declares_goal_met(&final_text) {
                    self.goal_runtime.write().await.complete();
                    break;
                }
                // Delta §11.6: budget check. The runtime flips to
                // BudgetLimited when either the token or the
                // wall-clock budget is spent; emit the steer once
                // and stop the loop.
                let (status, steer) = {
                    let mut rt = self.goal_runtime.write().await;
                    let steer = rt
                        .current_mut()
                        .map(|g| g.take_budget_steer())
                        .unwrap_or(false);
                    (rt.current().map(|g| g.status), steer)
                };
                if steer {
                    let _ = chunk_tx
                        .send(
                            "\n\n[goal budget exhausted — stopping. \
                             Raise the budget or drop the goal to continue.]\n"
                                .to_string(),
                        )
                        .await;
                }
                if status == Some(crate::goals::GoalStatus::BudgetLimited) {
                    all_text.push_str(
                        "\n\n(Goal loop stopped — budget exhausted. \
                         Raise the budget to continue.)",
                    );
                    break;
                }
                if turn == MAX_GOAL_TURNS {
                    all_text.push_str("\n\n(Goal loop stopped after maximum turns — progress above. Refine with /goal or /steer.)");
                }
            }
            self.remember_turn_for(key, false, &all_text).await;

            return Ok(TaskResponse {
                task_type: response.task_type,
                text: Some(all_text),
                tool_calls,
                tool_results,
                skills_used: refined_skills.clone(),
                memory_used: response.memory_used,
                execution_time_ms: response.execution_time_ms,
                usage: last_usage,
                // Goal-loop turns share one fallback chain; the
                // pricing that would report accurately is per-turn,
                // and the TUI's cost display uses the streaming
                // path, not the goal path. Left `None` rather than
                // guessed.
                pricing: None,
                memory_context: response.memory_context,
            });
        }

        Err(Self::no_provider_error())
    }
}
