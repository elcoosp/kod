use super::*;

impl KodEngine {
    /// Tier 3.3 — apply a same-endpoint retry strategy to a fresh
    /// request. Returns `true` when the adjustment succeeded; `false`
    /// means "this strategy cannot help here, fall through".
    ///
    /// The three adjusters are deliberately conservative:
    ///
    /// * `LowerTemp` halves the temperature (clamped at 0.0).
    /// * `Reinject` / `Constrained` prepend a system nudge to the
    ///   attempt's messages so the model sees what went wrong.
    /// * `ShrinkHistory` drops the oldest half of the messages.
    /// FNV-1a hash of the bytes that determine whether a provider's
    /// cache is still valid for a request.
    ///
    /// The "cacheable head" is the rendered cacheable system prefix
    /// plus the sorted tool schema bytes — everything the provider
    /// caches up to the marker. A change to any of it invalidates
    /// every endpoint's cache; the fingerprint captures that.
    ///
    /// Deterministic across runs: same prefix + same tools ⇒ same
    /// hash. The tools are sorted by name here even though the
    /// registry already sorts them, because the ledger must be
    /// robust to a caller that hands it an unsorted list.
    pub(crate) fn cache_head_fingerprint(
        system_text: &str,
        definitions: &[kod_types::ToolDefinition],
    ) -> u64 {
        const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
        const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
        let mut h = FNV_OFFSET;
        let mut mix = |bytes: &[u8]| {
            for b in bytes {
                h ^= *b as u64;
                h = h.wrapping_mul(FNV_PRIME);
            }
        };
        // Hash the whole system string. A change to *any* part of it,
        // volatile or cacheable, produces a new fingerprint; the
        // ledger then declares every endpoint cold, which is the
        // conservative direction. Splitting at the volatile marker
        // would be more precise but no more correct.
        mix(system_text.as_bytes());
        mix(&[0]);
        // Tool schemas: sort by name so a re-registration that changes
        // iteration order does not spuriously invalidate.
        let mut defs: Vec<&kod_types::ToolDefinition> = definitions.iter().collect();
        defs.sort_unstable_by(|a, b| a.name.cmp(&b.name));
        for def in defs {
            mix(def.name.as_bytes());
            mix(&[0]);
            mix(def.parameters_schema.to_string().as_bytes());
            mix(&[0]);
        }
        h
    }

    /// Feed a completed call into the cache ledger. Called from
    /// `record_cost`, the one place the engine already knows the
    /// winning endpoint, the turn, and the request head.
    pub(crate) fn ledger_observe(
        &self,
        endpoint: &str,
        head_fingerprint: u64,
        usage: &kod_provider::TokenUsage,
    ) {
        if let Ok(mut l) = self.cache_ledger.lock() {
            // Use a coarse "turn" derived from the process's monotonic
            // clock; the ledger only compares recency, never compares
            // across processes.
            let turn = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            l.observe(turn, endpoint, head_fingerprint, usage);
        }
    }

    /// Delta §2.4: record a settled usage report into the transcript's
    /// context gauge. Called from `record_cost_with_head`, which is
    /// the one place the engine already has the `usage` and the
    /// transcript key together.
    pub(crate) async fn gauge_observe(
        &self,
        holder: &str,
        covers_through: usize,
        usage: &kod_provider::TokenUsage,
    ) {
        let mut gauges = self.context_gauges.write().await;
        gauges
            .entry(holder.to_string())
            .or_default()
            .observe(covers_through, usage);
    }

    /// Delta §2.4: the anchored context-token estimate for `holder`,
    /// if an anchor exists. `tail_estimate` is the caller's estimate
    /// of the tokens added since the anchor; pass `0` for a readout
    /// that only wants to know "what did the provider last charge
    /// for?".
    ///
    /// `None` when no settled call has been observed for this
    /// transcript yet (the first turn of a session) or when the
    /// anchor was cleared by a compaction / forget / model switch.
    pub async fn context_tokens_for(&self, holder: &str, tail_estimate: u64) -> Option<u64> {
        self.context_gauges
            .read()
            .await
            .get(holder)
            .and_then(|g| g.estimate(tail_estimate))
    }

    /// Delta §2.4 (adoption): the anchored context-size estimate for
    /// `holder`, with a char-arithmetic tail for messages appended
    /// after the anchor.
    ///
    /// The anchor records the provider's own `prompt_tokens` on the
    /// last settled call. That number covers the system prompt,
    /// tool schemas, and every message up to the anchor index.
    /// Messages added since (the user's new prompt, tool round-trips
    /// from the previous turn) are the tail — the caller supplies
    /// their estimate.
    ///
    /// Returns `None` when there is no anchor: the first turn of a
    /// session, or after a compaction cleared the anchor. The
    /// caller falls back to `observed_usage` or the message-count
    /// heuristic in that case.
    pub(crate) async fn anchored_context_tokens(&self, holder: &str) -> Option<u64> {
        let tail_start = {
            let gauges = self.context_gauges.read().await;
            gauges.get(holder).and_then(|g| g.tail_start())?
        };
        let tail: u64 = {
            let history = self.history.read().await;
            match history.get(holder) {
                Some(turns) => turns
                    .iter()
                    .skip(tail_start)
                    .map(|m| (m.content.len() / 4) as u64)
                    .sum(),
                None => 0,
            }
        };
        self.context_tokens_for(holder, tail).await
    }

    /// Delta §4.1: try a mechanical compaction pass on `key` at
    /// `window_tokens`. Returns the plan when the dispatcher has work,
    /// `None` when no rung has anything to reduce.
    ///
    /// Holds the transcript read lock across the dispatcher call —
    /// both `ShakeMethod` and `PruneMethod` are pure functions that
    /// only read the transcript, so no write-lock contention is
    /// possible. `prefix_is_warm` is passed `false` for now: the
    /// engine's `CacheLedger` tracks warmth per-endpoint, not
    /// per-transcript, and computing it correctly is a follow-up. The
    /// conservative `false` skips the cache-warm guard, which errs on
    /// the side of *more* reduction — a deliberate bias for a first
    /// landing where the alternative is doing nothing.
    pub(crate) async fn try_mechanical_compaction(
        &self,
        key: &str,
        window_tokens: u64,
    ) -> Option<crate::compaction_dispatcher::CompactionPlan> {
        let guard = self.history.read().await;
        let turns = guard.get(key)?;
        if turns.is_empty() {
            return None;
        }

        // Precompute the suffix-token vector: `suffix[i]` is the
        // estimated token count of everything strictly after `turns[i]`.
        // The estimator is O(transcript) but a single pass, and it is
        // built once per dispatcher attempt (which only fires when the
        // threshold has been crossed).
        let mut suffix: Vec<u64> = vec![0u64; turns.len()];
        let mut acc = 0u64;
        for i in (0..turns.len()).rev() {
            suffix[i] = acc;
            acc += (turns[i].content.len() / 4) as u64;
        }
        let estimator = move |i: usize| suffix.get(i).copied().unwrap_or(0);

        // Build a provider handle for the LLM-calling methods. None
        // when no provider is installed — those methods report
        // `Unavailable` and the dispatcher falls through to the
        // mechanical rungs.
        let provider_handle = match self.current_provider().await {
            Some(p) => {
                let options = self.generation_defaults.read().await.to_options();
                let model = self.current_model.read().await.clone();
                Some(crate::compaction_dispatcher::ProviderHandle {
                    provider: p,
                    options,
                    model,
                })
            }
            None => None,
        };
        // Delta §11.10: the plan-protected paths. Cloned out of the
        // RwLock before the lock is dropped — the ctx borrows it for
        // the duration of the dispatcher call.
        let protected_paths = self.plan_reference_paths(key).await;
        let ctx = crate::compaction_dispatcher::CompactionContext {
            transcript: turns,
            window_tokens,
            suffix_tokens_after: &estimator,
            prefix_is_warm: false,
            provider: provider_handle,
            protected_paths: &protected_paths,
        };
        let outcome = self.compaction_dispatcher.compact(&ctx).await;
        // Notices from skipped stubs are dropped here — the engine has
        // no user-visible channel for them yet, and a per-turn notice
        // storm would be the same failure the advisor guard (§11.8)
        // exists to prevent. A `/debug` surface is a follow-up.
        let _ = outcome.notices;
        outcome.plan
    }

    /// Delta §4.1: apply a mechanical compaction plan to `key`.
    ///
    /// Returns the number of message mutations applied. `Shake` plans
    /// carry per-message byte ranges already ordered descending by
    /// start (the planner's contract), so splicing them in the order
    /// given leaves earlier offsets valid. `Prune` plans blank whole
    /// message bodies. The three summary-family variants
    /// (`Summary`, `NativeSummary`, `Image`) drain the covered
    /// prefix and insert a pinned marker in its place; a provider
    /// block (native) or a rasterized frame (image) is stored
    /// under the transcript key so the next request carries it.
    pub(crate) async fn apply_compaction_plan(
        &self,
        key: &str,
        plan: crate::compaction_dispatcher::CompactionPlan,
        window_tokens: u64,
    ) -> usize {
        use crate::compaction_dispatcher::CompactionPlan;
        use kod_core_quality::prune::PruneAction;
        use std::collections::HashMap;

        let mut history = self.history.write().await;
        let Some(turns) = history.get_mut(key) else {
            return 0;
        };

        // Delta §4.3: the no-reduction guard's inputs. `current` is
        // measured locally with the same chars/4 convention every
        // planner uses; `reserve` is the same `max(15%, 16_384)` the
        // dispatcher's `should_compact` applies. Both are read before
        // any arm mutates the transcript.
        //
        // Shake and Prune are exempt: a placeholder is by construction
        // shorter than the body it replaces (the config's
        // `min_tokens` / `min_prune_tokens` gate rejects the candidate
        // before the plan is produced), so the guard would only fire
        // on a plan that is already a no-op. Summary-family plans are
        // not: a summarizer asked to reduce a transcript can, in the
        // worst case, produce a summary longer than what it replaces.
        let current_tokens: u64 = turns.iter().map(|m| (m.content.len() / 4) as u64).sum();
        let reserve = crate::compaction_dispatcher::resolve_reserve(window_tokens);

        // Snapshot the id → index map once. Message ids are unique
        // within a transcript by construction.
        let idx: HashMap<kod_types::MessageId, usize> = turns
            .iter()
            .enumerate()
            .map(|(i, m)| (m.id.clone(), i))
            .collect();

        let mut affected = 0usize;
        match plan {
            CompactionPlan::Shake(shake) => {
                for action in shake.actions {
                    let Some(&i) = idx.get(&action.message_id) else {
                        continue;
                    };
                    let content = &mut turns[i].content;
                    let start = action.range.start.min(content.len());
                    let end = action.range.end.min(content.len());
                    if start >= end {
                        continue;
                    }
                    if !content.is_char_boundary(start) || !content.is_char_boundary(end) {
                        // A byte range that is not on a char boundary
                        // would panic `replace_range`; skip rather
                        // than trust the planner's arithmetic when a
                        // non-ASCII transcript has been edited under
                        // it.
                        continue;
                    }
                    content.replace_range(start..end, &action.placeholder);
                    affected += 1;
                }
            }
            CompactionPlan::Prune(prune) => {
                for (id, action) in prune.actions {
                    let Some(&i) = idx.get(&id) else { continue };
                    match action {
                        PruneAction::Blank { placeholder } => {
                            turns[i].content = placeholder;
                            affected += 1;
                        }
                    }
                }
            }
            CompactionPlan::NativeSummary {
                covers_through,
                text,
                encrypted_content,
            } => {
                // Delta §4.3: no-reduction guard BEFORE any state
                // mutation. A rejected plan leaves the transcript and
                // the native-block store untouched.
                if turns.is_empty() {
                    return 0;
                }
                let end = (covers_through + 1).min(turns.len());
                if end == 0 {
                    return 0;
                }
                let summary_body = format!("## Previous Conversation Handoff\n{text}");
                let projected = (summary_body.len() / 4) as u64
                    + turns[end..]
                        .iter()
                        .map(|m| (m.content.len() / 4) as u64)
                        .sum::<u64>();
                let admission = crate::compaction_dispatcher::CompactionAdmission {
                    projected,
                    current: current_tokens,
                    window: window_tokens,
                    reserve,
                };
                if !admission.admits() {
                    tracing::warn!(
                        projected,
                        current = current_tokens,
                        window = window_tokens,
                        reserve,
                        "native compaction plan rejected: not a reduction",
                    );
                    return 0;
                }
                // Same drain-and-replace as `Summary`, plus store the
                // opaque block so the next request for this
                // transcript carries it. The block lives under the
                // transcript key, not in the message — a message
                // that carried it would be re-sent verbatim on every
                // turn, growing the transcript it is trying to
                // shrink.
                if !encrypted_content.is_empty() {
                    self.native_compaction_blocks
                        .write()
                        .await
                        .insert(key.to_string(), encrypted_content);
                }
                let dropped: Vec<kod_types::ChatMessage> = turns.drain(..end).collect();
                let mut summary_msg = kod_types::ChatMessage::text(
                    kod_types::MessageId::new(),
                    kod_types::MessageRole::User,
                    summary_body,
                    time::OffsetDateTime::now_utc(),
                );
                summary_msg.metadata.pinned = true;
                turns.insert(0, summary_msg);
                affected = dropped.len();
            }
            CompactionPlan::Image {
                covers_through,
                png_base64,
                source_lines,
            } => {
                // Delta §4.5: replace the older half with a short
                // marker and attach the frame on subsequent requests.
                //
                // Delta §4.3: the no-reduction guard runs BEFORE the
                // frame store so a rejected plan does not leave a
                // stray frame attached to a transcript it did not
                // shrink.
                if png_base64.is_empty() {
                    return 0;
                }
                if turns.is_empty() {
                    return 0;
                }
                let end = (covers_through + 1).min(turns.len());
                if end == 0 {
                    return 0;
                }
                let marker_body = format!(
                    "## Previous conversation rendered as image frame\n\
                     ({source_lines} lines rasterized; the image is \
                     attached to this request.)",
                );
                let projected = (marker_body.len() / 4) as u64
                    + turns[end..]
                        .iter()
                        .map(|m| (m.content.len() / 4) as u64)
                        .sum::<u64>();
                let admission = crate::compaction_dispatcher::CompactionAdmission {
                    projected,
                    current: current_tokens,
                    window: window_tokens,
                    reserve,
                };
                if !admission.admits() {
                    tracing::warn!(
                        projected,
                        current = current_tokens,
                        window = window_tokens,
                        reserve,
                        "image compaction plan rejected: not a reduction",
                    );
                    return 0;
                }
                self.image_frames
                    .write()
                    .await
                    .entry(key.to_string())
                    .or_default()
                    .push(kod_provider::request::ImageFrame {
                        png_base64,
                        media_type: "image/png".to_string(),
                    });
                let dropped: Vec<kod_types::ChatMessage> = turns.drain(..end).collect();
                let mut marker_msg = kod_types::ChatMessage::text(
                    kod_types::MessageId::new(),
                    kod_types::MessageRole::User,
                    marker_body,
                    time::OffsetDateTime::now_utc(),
                );
                marker_msg.metadata.pinned = true;
                turns.insert(0, marker_msg);
                affected = dropped.len();
            }
            CompactionPlan::Summary {
                covers_through,
                text,
            } => {
                // Delta §4.1: the handoff rung produces this plan.
                // The summary replaces the first `covers_through + 1`
                // messages: they are the older half the handoff
                // document stands in for. The newer half (the
                // working set) is untouched, which is the entire
                // reason a handoff is preferred to a full summary
                // when it can be made.
                //
                // Clamp: the plan is computed under a read guard and
                // applied under the write guard, so the transcript
                // could in principle have shrunk (a concurrent
                // forget, a session reset). Clamping to
                // `len - 1` means we never drain more than exists.
                if turns.is_empty() {
                    return 0;
                }
                let end = (covers_through + 1).min(turns.len());
                if end == 0 {
                    return 0;
                }
                // Delta §4.3: no-reduction guard. Measured with the
                // same chars/4 convention every planner uses; a
                // rejected plan leaves the transcript untouched so
                // the dispatcher can fall through to the next rung
                // (and a caller with no further rung sees the
                // existing context-full handling).
                let summary_body = format!("## Previous Conversation Handoff\n{text}");
                let projected = (summary_body.len() / 4) as u64
                    + turns[end..]
                        .iter()
                        .map(|m| (m.content.len() / 4) as u64)
                        .sum::<u64>();
                let admission = crate::compaction_dispatcher::CompactionAdmission {
                    projected,
                    current: current_tokens,
                    window: window_tokens,
                    reserve,
                };
                if !admission.admits() {
                    tracing::warn!(
                        projected,
                        current = current_tokens,
                        window = window_tokens,
                        reserve,
                        "compaction summary rejected: not a reduction",
                    );
                    return 0;
                }
                let dropped: Vec<kod_types::ChatMessage> = turns.drain(..end).collect();
                let mut summary_msg = kod_types::ChatMessage::text(
                    kod_types::MessageId::new(),
                    kod_types::MessageRole::User,
                    summary_body,
                    time::OffsetDateTime::now_utc(),
                );
                // Pinned: the render path never drops the handoff
                // for budget reasons — losing it would lose the only
                // record of what it replaced.
                summary_msg.metadata.pinned = true;
                turns.insert(0, summary_msg);
                affected = dropped.len();
            }
        }
        affected
    }

    pub(crate) fn apply_retry_adjustment(
        action: kod_core_routing::retry_strategy::RetryAction,
        options: &mut GenerationOptions,
        messages: &mut Vec<kod_types::ChatMessage>,
    ) -> bool {
        use kod_core_routing::retry_strategy::RetryAction as A;
        match action {
            A::SameEndpointLowerTemp => {
                options.temperature = Some((options.temperature.unwrap_or(0.7) * 0.5).max(0.0));
                true
            }
            A::ReinjectTools => {
                messages.push(kod_types::ChatMessage::text(
                    kod_types::MessageId::new(),
                    kod_types::MessageRole::System,
                    "Your previous reply named a tool that does not exist. \
                     Re-read the tool inventory above and only use tools \
                     listed there."
                        .to_string(),
                    time::OffsetDateTime::now_utc(),
                ));
                true
            }
            A::SameEndpointConstrained => {
                messages.push(kod_types::ChatMessage::text(
                    kod_types::MessageId::new(),
                    kod_types::MessageRole::System,
                    "Your previous reply did not parse. Respond again, \
                     being careful to produce valid JSON for any tool \
                     call, matching the schema exactly."
                        .to_string(),
                    time::OffsetDateTime::now_utc(),
                ));
                true
            }
            A::ShrinkHistory => {
                if messages.len() < 4 {
                    return false;
                }
                let keep = messages.len() / 2;
                let drop = messages.len() - keep;
                messages.drain(0..drop);
                true
            }
            _ => false,
        }
    }

    /// Extract a JSON array of step strings from a model reply that may
    /// carry prose around it (Tier 2.1). Tolerant: first `[` to last `]`,
    /// every element coerced to a string.
    pub(crate) fn parse_plan_steps(text: &str) -> Option<Vec<String>> {
        let start = text.find('[')?;
        let end = text.rfind(']')?;
        if end <= start {
            return None;
        }
        let v: serde_json::Value = serde_json::from_str(&text[start..=end]).ok()?;
        let arr = v.as_array()?;
        let steps: Vec<String> = arr
            .iter()
            .filter_map(|x| {
                x.as_str()
                    .map(String::from)
                    .or_else(|| x.as_str().map(String::from))
            })
            .filter(|s| !s.trim().is_empty())
            .collect();
        if steps.is_empty() { None } else { Some(steps) }
    }

    /// P2-a: compact `key`'s transcript before the next render when
    /// the observed token count has crossed the hard threshold.
    ///
    /// Called from `prepare_turn` immediately before
    /// `render_history_for`, so the compacted transcript is what gets
    /// rendered and the current turn pays the smaller prompt. Returns
    /// the number of messages dropped (0 when no compaction ran).
    ///
    /// This is the *emergency* path only: it drops the oldest safe
    /// block and inserts a factual no-LLM summary. The background
    /// LLM summarization that replaces the dropped block with real
    /// prose is a follow-up; what matters here is that the transcript
    /// never grows past the window and never splits a tool pair.
    /// P2-a: spawn the background summarization for a transcript
    /// whose size has crossed the soft threshold.
    ///
    /// The task clones the provider (an `Arc`, cheap) and the two
    /// `Arc`-shared state maps, builds a prompt from the block that
    /// would be dropped, and writes the result to `pending_summaries`.
    /// When the hard threshold is crossed on a later turn,
    /// `maybe_compact_for` uses that text in place of the emergency
    /// summary: real prose rather than counts and file names.
    ///
    /// Fire-and-forget. A failure clears the in-flight mark so the
    /// next soft-threshold turn retries; the emergency path still
    /// bounds the context meanwhile, which is the property that has
    /// to hold.
    pub(crate) async fn spawn_compaction_summary(
        &self,
        key: &str,
        dropped: Vec<kod_types::ChatMessage>,
    ) {
        // One task per transcript; a second call while one is running
        // is a no-op rather than a duplicate request.
        {
            let mut in_flight = self.summaries_in_flight.write().await;
            if !in_flight.insert(key.to_string()) {
                return;
            }
        }

        let Some(provider) = self.current_provider().await else {
            self.summaries_in_flight.write().await.remove(key);
            return;
        };
        let options = self.generation_defaults.read().await.to_options();
        let prompt = build_summary_prompt(&dropped);

        // Clone the shared state; the task never touches `self`.
        let pending = self.pending_summaries.clone();
        let in_flight = self.summaries_in_flight.clone();
        let engine_key = key.to_string();

        tokio::spawn(async move {
            let result = provider.generate(&prompt, &options).await;
            match result {
                Ok(text) if !text.trim().is_empty() => {
                    pending
                        .write()
                        .await
                        .insert(engine_key.clone(), text.trim().to_string());
                }
                Ok(_) => {
                    tracing::warn!(key = %engine_key,
                        "compaction summary returned empty; emergency path will be used");
                }
                Err(e) => {
                    tracing::warn!(key = %engine_key, error = %e,
                        "compaction summary failed; emergency path will be used");
                }
            }
            in_flight.write().await.remove(&engine_key);
        });
    }

    pub(crate) async fn maybe_compact_for(&self, key: &str) -> usize {
        // Budget from the cached window; the `0` case (no registry
        // installed) makes `decide` return `None` and nothing runs.
        let (window, _max_out) = self.budget_hint.read().map(|g| *g).unwrap_or((0, 0));
        if window == 0 {
            return 0;
        }

        // Delta §2.4 (adoption): prefer the provider-anchored gauge.
        // Its value is the anchor's `prompt_tokens` (the exact number
        // the provider last charged for — system prompt, tools, and
        // messages) plus a char-arithmetic tail for messages appended
        // since. The `observed_usage` fallback below records only
        // `prompt + completion` from the last call, so it under-counts
        // by whatever the provider charged for the prompt *scaffolding*
        // (identity block, tool schemas, repo map). The gauge is the
        // more accurate of the two; the fallback exists for the first
        // turn of a session, when no anchor has been established yet,
        // and for a resumed session whose transcript was loaded from
        // disk without a preceding call.
        let used = match self.anchored_context_tokens(key).await {
            Some(n) => n,
            None => match self.observed_usage.read().await.get(key) {
                Some(&n) => n,
                None => {
                    let guard = self.history.read().await;
                    let chars: usize = guard
                        .get(key)
                        .map(|t| t.iter().map(|m| m.content.len()).sum())
                        .unwrap_or(0);
                    (chars / 4) as u64
                }
            },
        };

        let action = crate::compaction::decide(used, window as u64);
        if action == crate::compaction::Action::None {
            return 0;
        }

        // Delta §4.1: reduction before summarization. Try a mechanical
        // pass first — shake and prune are pure functions that do not
        // need the model, and a successful plan avoids the summary
        // call entirely. Only when the mechanical rungs find nothing
        // (or fall short) does the summary path run.
        if let Some(plan) = self.try_mechanical_compaction(key, window as u64).await {
            let affected = self.apply_compaction_plan(key, plan, window as u64).await;
            if affected > 0 {
                tracing::info!(
                    holder = key,
                    affected,
                    "mechanical compaction reduced the transcript before summarization",
                );
                return affected;
            }
        }

        // Compute the cut once, using a read guard, so both the
        // StartBackground and CompactNow paths agree on which
        // messages are in play.
        let (_cut, dropped_for_summary) = {
            let guard = self.history.read().await;
            let Some(turns) = guard.get(key) else {
                return 0;
            };
            let Some(cut) =
                crate::compaction::safe_cutoff(turns, crate::compaction::RECENT_TURNS_TO_KEEP)
            else {
                return 0;
            };
            if cut == 0 {
                // No safe cut drops anything: one enormous
                // un-splittable block. Leave it; the request is
                // rejected and the caller sees the real limit.
                return 0;
            }
            (cut, turns[..cut].to_vec())
        };

        if action == crate::compaction::Action::StartBackground {
            // Queue a background summary for the block that *would*
            // be dropped. It lands in `pending_summaries` for the turn
            // that later crosses the hard threshold.
            self.spawn_compaction_summary(key, dropped_for_summary)
                .await;
            return 0;
        }

        // CompactNow: take the pending summary if the background task
        // has finished, else fall back to the emergency text.
        let background_summary = self.pending_summaries.write().await.remove(key);

        let mut guard = self.history.write().await;
        let Some(turns) = guard.get_mut(key) else {
            return 0;
        };
        // Recompute against the live transcript; it may have grown
        // since the read-guard pass above.
        let Some(cut) =
            crate::compaction::safe_cutoff(turns, crate::compaction::RECENT_TURNS_TO_KEEP)
        else {
            return 0;
        };
        if cut == 0 {
            return 0;
        }
        // T3-H6: rotate-then-truncate keeps the retained suffix in place
        // rather than memmoving the whole tail forward like drain does.
        let dropped: Vec<kod_types::ChatMessage> = {
            let dropped: Vec<kod_types::ChatMessage> = turns[..cut].to_vec();
            turns.rotate_left(cut);
            turns.truncate(turns.len() - cut);
            dropped
        };
        let summary = background_summary
            .unwrap_or_else(|| crate::compaction::emergency_summary(&dropped, window as u64));
        let mut summary_msg = kod_types::ChatMessage::text(
            kod_types::MessageId::new(),
            kod_types::MessageRole::User,
            format!("## Previous Conversation Summary\n{summary}"),
            time::OffsetDateTime::now_utc(),
        );
        // Pinned so the render path never drops the summary for
        // budget reasons — losing it would lose the only record of
        // what was compacted away.
        summary_msg.metadata.pinned = true;
        turns.insert(0, summary_msg);

        tracing::info!(
            key,
            dropped = cut,
            used_tokens = used,
            window,
            "P2-a: emergency compaction ran",
        );
        cut
    }
}
