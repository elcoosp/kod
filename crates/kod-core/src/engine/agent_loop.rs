use super::*;

impl KodEngine {
    /// Collected (non-streaming) agentic loop used by [`process`].
    pub(crate) async fn run_collected_loop(
        &self,
        provider: &Arc<dyn LlmProvider>,
        pending: &mut String,
        messages: &mut Vec<kod_types::ChatMessage>,
        round: &RoundContext<'_>,
    ) -> Result<(
        String,
        Vec<ToolCall>,
        Vec<ToolResult>,
        Option<kod_provider::TokenUsage>,
    )> {
        let mut final_text = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut tool_results: Vec<ToolResult> = Vec::new();
        let mut last_usage: Option<kod_provider::TokenUsage> = None;
        for _ in 0..MAX_TOOL_ROUNDS {
            if self.is_cancelled_for(round.holder) {
                return Err(KodError::InvalidState("cancelled by user".to_string()));
            }
            // Delta §9.8: the pause gate's model-call boundary.
            self.pause_gate.wait_if_paused().await;
            // Pre-queued steers reach round 1. `apply_steers`
            // also runs after a tool round; both calls are safe
            // because it drains.
            self.apply_steers(pending, messages, round.holder).await;
            // Rebuild the structured request every round. Only the
            // `messages` field changes; the system prompt, tools,
            // options and model are constant for the turn.
            // Tier 1.3 — redact before grounding. No-op by default.
            let _ = self.redact_messages_for_prompt(messages);
            // Delta §4.5: rasterize large text tool results before
            // the request is built. No-op when the provider has no
            // vision capability or no result crosses the threshold.
            self.inline_image_tool_results(messages).await;

            let req = self
                .build_grounded_request(
                    round.holder,
                    round.system_text,
                    messages.clone(),
                    round.definitions,
                    round.options,
                    round.model_ref,
                )
                .await;
            match provider.complete(&req).await? {
                GenerationResponse::Text { content, usage } => {
                    last_usage = match (last_usage, usage) {
                        (Some(prev), Some(next)) => Some(prev.merge(&next)),
                        (Some(prev), None) => Some(prev),
                        (None, Some(next)) => Some(next),
                        (None, None) => None,
                    };
                    append_round_text(&mut final_text, &content);
                    break;
                }
                GenerationResponse::ToolCalls { calls, usage } => {
                    last_usage = match (last_usage, usage) {
                        (Some(prev), Some(next)) => Some(prev.merge(&next)),
                        (Some(prev), None) => Some(prev),
                        (None, Some(next)) => Some(next),
                        (None, None) => None,
                    };
                    if calls.is_empty() {
                        break;
                    }
                    // Delta §14.1: deobfuscate placeholders in each
                    // tool call's arguments before the tool runs.
                    // Same pattern as the streaming path; the
                    // `calls` binding is mutable so the local copy
                    // carries raw values while the transcript's
                    // assistant message keeps the placeholder.
                    let mut calls = calls;
                    for call in calls.iter_mut() {
                        let _ = self.deobfuscate_json(&mut call.arguments).await;
                    }
                    let section = self.run_tool_calls(&calls, round.holder, None).await;
                    // Delta §9.4: check for a repeated tool round and,
                    // if the guard fires, append a corrective System
                    // message before the loop continues. Runs on both
                    // ToolCalls and Mixed arms; the guard's per-key
                    // state means a Mixed-then-ToolCalls sequence is
                    // observed as a single stream of rounds.
                    self.maybe_emit_loop_corrective(
                        round.holder,
                        &calls,
                        &section.results,
                        messages,
                    )
                    .await;
                    // Delta §11.7: a mid-run todo reconcile nudge when
                    // the tracker says the model has drifted from its
                    // plan.
                    self.maybe_emit_todo_nudge(round.holder, &calls, messages)
                        .await;
                    tool_calls.extend(calls);
                    tool_results.extend(section.results.clone());
                    messages.extend(section.messages.iter().cloned());
                    // Keep the text prompt in sync for callers that
                    // still read `pending` (apply_steers, the
                    // exhausted-rounds note). The provider no longer
                    // sees this string on the primary path.
                    pending.push_str(&format!("\n\n{}", section.prompt_block));
                    // Persist the structured slice (AD-02): the next
                    // `process_*` call reads `self.history[key]` as
                    // its starting messages, so the tool round-trip
                    // has to survive past this call to be visible
                    // there.
                    if !section.messages.is_empty() {
                        let mut hist = self.history.write().await;
                        let turns = hist.entry(round.holder.to_string()).or_default();
                        turns.extend(section.messages.iter().cloned());
                        cap_transcript(&mut *turns, MAX_HISTORY_TURNS);
                    }
                    self.apply_steers(pending, messages, round.holder).await;
                }
                GenerationResponse::Mixed {
                    content,
                    calls,
                    usage,
                } => {
                    last_usage = match (last_usage, usage) {
                        (Some(prev), Some(next)) => Some(prev.merge(&next)),
                        (Some(prev), None) => Some(prev),
                        (None, Some(next)) => Some(next),
                        (None, None) => None,
                    };
                    append_round_text(&mut final_text, &content);
                    if calls.is_empty() {
                        break;
                    }
                    // Delta §14.1: deobfuscate placeholders in each
                    // tool call's arguments before the tool runs.
                    // Same pattern as the streaming path; the
                    // `calls` binding is mutable so the local copy
                    // carries raw values while the transcript's
                    // assistant message keeps the placeholder.
                    let mut calls = calls;
                    for call in calls.iter_mut() {
                        let _ = self.deobfuscate_json(&mut call.arguments).await;
                    }
                    let section = self.run_tool_calls(&calls, round.holder, None).await;
                    // Delta §9.4: check for a repeated tool round and,
                    // if the guard fires, append a corrective System
                    // message before the loop continues. Runs on both
                    // ToolCalls and Mixed arms; the guard's per-key
                    // state means a Mixed-then-ToolCalls sequence is
                    // observed as a single stream of rounds.
                    self.maybe_emit_loop_corrective(
                        round.holder,
                        &calls,
                        &section.results,
                        messages,
                    )
                    .await;
                    // Delta §11.7: a mid-run todo reconcile nudge when
                    // the tracker says the model has drifted from its
                    // plan.
                    self.maybe_emit_todo_nudge(round.holder, &calls, messages)
                        .await;
                    tool_calls.extend(calls);
                    tool_results.extend(section.results.clone());
                    messages.extend(section.messages.iter().cloned());
                    pending.push_str(&format!("\n\n{}", section.prompt_block));
                    // Persist the structured slice (AD-02): the next
                    // `process_*` call reads `self.history[key]` as
                    // its starting messages, so the tool round-trip
                    // has to survive past this call to be visible
                    // there.
                    if !section.messages.is_empty() {
                        let mut hist = self.history.write().await;
                        let turns = hist.entry(round.holder.to_string()).or_default();
                        turns.extend(section.messages.iter().cloned());
                        cap_transcript(&mut *turns, MAX_HISTORY_TURNS);
                    }
                    self.apply_steers(pending, messages, round.holder).await;
                }
            }
        }
        // Exited on the round cap rather than a text-only reply. Tell
        // the transcript — the caller asks the model for a summary
        // after this returns, and this note is what makes that
        // summary "what got done" rather than a recap of the last
        // tool result.
        if final_text.trim().is_empty() && !tool_calls.is_empty() {
            pending.push_str(TOOL_ROUNDS_EXHAUSTED_NOTE);
            messages.push(kod_types::ChatMessage::text(
                kod_types::MessageId::new(),
                kod_types::MessageRole::User,
                TOOL_ROUNDS_EXHAUSTED_NOTE.trim().to_string(),
                time::OffsetDateTime::now_utc(),
            ));
        }
        Ok((final_text, tool_calls, tool_results, last_usage))
    }

    /// Append queued steer notes for `key` to the running conversation
    /// (each once). Called inside the three agentic loops with the
    /// loop's own `holder`.
    ///
    /// Two destinations, on purpose:
    ///
    /// - `messages`: a structured `User` message. This is what the
    ///   provider actually receives on the AD-01 path. A regression
    ///   that only appended to `pending` (the pre-migration shape)
    ///   left the steer invisible to the model; the test in
    ///   `crates/kod-core/tests/steers_reach_the_provider.rs` pins
    ///   the structured form.
    /// - `pending`: the text trace. Kept so `/debug last-prompt`
    ///   shows the steer in the exact position the pre-migration
    ///   path would have placed it, which is what users have learned
    ///   to read.
    pub(crate) async fn apply_steers(
        &self,
        pending: &mut String,
        messages: &mut Vec<kod_types::ChatMessage>,
        key: &str,
    ) {
        // Delta §11.4: fold any batched async-job results into the
        // steer queue before draining it. The queue is owner-routed
        // and epoch-guarded; a result whose session moved on was
        // already dropped. Doing this here means every round boundary
        // that already applies steers also picks up finished-job
        // results, with no second call site to forget.
        {
            let msg = self.async_delivery.lock().drain(key);
            if let Some(m) = msg {
                let mut q = self.steers.write().await;
                q.entry(key.to_string())
                    .or_default()
                    .push(kod_core_state::steer::SoftInterrupt::background(m));
            }
        }
        for interrupt in self.take_steers_for(key).await {
            // The header is `SoftInterrupt::render`'s job now. For a
            // User-source interrupt the rendered text is byte-identical
            // to the pre-P1-b string (`steer.rs` pins that), so the
            // golden prompts and recorded transcripts are unchanged.
            let body = interrupt.render();
            pending.push_str(&format!("\n\n{body}\n"));
            messages.push(kod_types::ChatMessage::text(
                kod_types::MessageId::new(),
                kod_types::MessageRole::User,
                body,
                time::OffsetDateTime::now_utc(),
            ));
        }
    }

    /// M-16: current length of a transcript's history.
    pub(crate) async fn history_len_for(&self, key: &str) -> usize {
        self.history
            .read()
            .await
            .get(key)
            .map(|v| v.len())
            .unwrap_or(0)
    }

    /// M-16: truncate a transcript's history back to `len` messages.
    pub(crate) async fn truncate_history_to(&self, key: &str, len: usize) {
        let mut hist = self.history.write().await;
        if let Some(turns) = hist.get_mut(key)
            && turns.len() > len
        {
            turns.truncate(len);
        }
    }

    /// Streaming agentic loop: text chunks are forwarded to `chunk_tx` the
    /// moment they arrive; tool-start markers go through the same channel.
    pub(crate) async fn run_streaming_loop(
        &self,
        provider: &Arc<dyn LlmProvider>,
        pending: &mut String,
        messages: &mut Vec<kod_types::ChatMessage>,
        chunk_tx: &tokio::sync::mpsc::Sender<String>,
        round: &RoundContext<'_>,
    ) -> Result<(
        String,
        Vec<ToolCall>,
        Vec<ToolResult>,
        Option<kod_provider::TokenUsage>,
        // P5.6 — Jev flagged the round off-track and the caller may
        // want to retry against the next endpoint. Only ever true on
        // round 0 (before any tool call has run).
        bool,
    )> {
        let mut final_text = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut tool_results: Vec<ToolResult> = Vec::new();
        let mut last_usage: Option<kod_provider::TokenUsage> = None;
        // Per-round routing (P1.3). `current_provider` and
        // `current_model_ref` are the round's effective provider and
        // model; the loop starts with the chain-resolved choice and
        // swaps to whatever `pick_round_endpoint` returns.
        let mut current_provider: Arc<dyn LlmProvider> = provider.clone();
        let mut current_model_ref: ModelRef = round.model_ref.clone();
        let mut had_tool_results = false;
        for round_idx in 0..MAX_TOOL_ROUNDS {
            // Delta §9.11: per-round wall time, recorded into the run
            // collector after the round's outcome is known.
            let round_started = std::time::Instant::now();
            if self.is_cancelled_for(round.holder) {
                return Err(KodError::InvalidState("cancelled by user".to_string()));
            }
            // Delta §9.8: the pause gate's model-call boundary.
            self.pause_gate.wait_if_paused().await;
            // Pre-queued steers reach round 1. See the same comment in
            // `run_collected_loop`.
            self.apply_steers(pending, messages, round.holder).await;

            // Ask Jev what kind of round this is and whether a
            // different endpoint should serve it. A `None` result is
            // the common case (no routing configured) and leaves the
            // chain-resolved choice in place.
            if let Some(next) = self
                .pick_round_endpoint(round.holder, round_idx, had_tool_results)
                .await
            {
                match self.resolve_provider_for_model_ref(&next).await {
                    Ok(p) => {
                        current_provider = p;
                        current_model_ref = next;
                    }
                    Err(e) => {
                        tracing::warn!(
                            endpoint = %next.endpoint,
                            error = %e,
                            "round-routed endpoint did not resolve; keeping current"
                        );
                    }
                }
            }

            // Tier 1.3 — redact the outgoing messages before the
            // provider sees them. No-op when `[security.redact]
            // in_prompt = false` (the default).
            let _ = self.redact_messages_for_prompt(messages);
            // Delta §4.5: rasterize large text tool results.
            self.inline_image_tool_results(messages).await;

            // Rebuild a RoundContext for this round so the
            // effective model_ref is visible to the grounded
            // request and the tool calls below.
            let round_for_this = RoundContext {
                system_text: round.system_text,
                model_ref: &current_model_ref,
                definitions: round.definitions,
                options: round.options,
                holder: round.holder,
                trace: None,
                // P5.6 — carry the outer round's fallback through to
                // the inner `stream_round` call so mid-stream
                // switching has a target.
                fallback: round.fallback,
            };
            let StreamRoundOutcome {
                text,
                mut calls,
                usage,
                retry_suggested: off_track,
                speculations,
                stop_reason,
                partial_error,
            } = self
                .stream_round(
                    &current_provider,
                    round_for_this.system_text,
                    messages,
                    round_for_this.model_ref,
                    round_for_this.definitions,
                    round_for_this.options,
                    chunk_tx,
                    round_for_this.holder,
                    round_for_this.trace,
                    // P5.6 — the next chain endpoint for mid-stream
                    // switching. `None` when this is the last entry,
                    // which preserves the pre-P5.6 behaviour
                    // (retry_suggested -> outer chain loop).
                    round_for_this.fallback,
                )
                .await?;
            // Delta §9.11: record this round's outcome for /stats.
            // The stop reason is not threaded out of stream_round
            // today (the provider's StopReason chunk is logged, not
            // returned), so the turn is recorded with a None reason
            // and the cost-unavailable signal derived from usage.
            {
                let elapsed = round_started.elapsed().as_millis() as u64;
                let cost_unavailable = match usage.as_ref() {
                    None => Some(crate::run_collector::CostUnavailable::NoUsage),
                    Some(_) => None,
                };
                self.run_collector.lock().observe_turn(
                    stop_reason.as_deref(),
                    elapsed,
                    cost_unavailable,
                );
            }
            // M-13: a mid-stream error is carried alongside the round's
            // partial text and complete calls. Surface it now — the
            // chunks already reached the TUI live, and the caller sees
            // the error rather than a silently-truncated success.
            if let Some(err) = partial_error {
                tracing::warn!(
                    error = %err,
                    text_len = text.len(),
                    calls = calls.len(),
                    "stream round ended with a partial error",
                );
                // M-13: persist what the user already saw. The chunks
                // reached the TUI live; without this the transcript
                // recorded nothing and the next turn re-asked a
                // question the model had already half-answered.
                if !text.trim().is_empty() {
                    let msg = kod_types::ChatMessage::text(
                        kod_types::MessageId::new(),
                        kod_types::MessageRole::Assistant,
                        text.clone(),
                        time::OffsetDateTime::now_utc(),
                    );
                    messages.push(msg.clone());
                    let mut hist = self.history.write().await;
                    hist.entry(round.holder.to_string())
                        .or_default()
                        .push(msg);
                }
                // M-13 remainder: the round assembled complete tool
                // calls before the stream died. When this error is NOT
                // retryable, the turn is ending here — no fallback will
                // re-drive it — so executing the calls now is safe and
                // gives the user the work the model actually asked for.
                //
                // When the error IS retryable the chain may fall back
                // and re-drive the turn; the model re-emits the calls
                // and they execute there. Executing here as well would
                // run the same tools twice — real side effects, not a
                // transcript blemish — so we deliberately skip.
                if !err.is_retryable()
                    && !calls.is_empty()
                {
                    let section = self
                        .run_tool_calls_with_speculations(
                            &calls,
                            round.holder,
                            Some(chunk_tx),
                            &speculations,
                        )
                        .await;
                    if !section.messages.is_empty() {
                        messages.extend(section.messages.iter().cloned());
                        let mut hist = self.history.write().await;
                        let turns = hist.entry(round.holder.to_string()).or_default();
                        turns.extend(section.messages.iter().cloned());
                        cap_transcript(&mut *turns, MAX_HISTORY_TURNS);
                    }
                }
                return Err(err);
            }
            // P5.6 — on the very first round, an off-track verdict
            // is a hard stop: discard the round's text and signal
            // the caller to try the next endpoint. Emit the reset
            // marker so the TUI drops what it displayed.
            if off_track && round_idx == 0 {
                let _ = chunk_tx.send(stream_reset_marker()).await;
                // Drop the speculations on the floor: the calls they
                // were admitted for are being discarded too.
                drop(speculations);
                return Ok((String::new(), Vec::new(), Vec::new(), None, true));
            }
            // Live window snapshot for the meter: this round's
            // provider numbers supersede the previous round's. (The
            // merged total below is for session accounting and cost —
            // it sums history once per round and must never drive the
            // context meter. See USAGE_MARKER.)
            if let Some(u) = usage.as_ref() {
                let _ = chunk_tx
                    .send(usage_marker(u.prompt_tokens, u.completion_tokens))
                    .await;
            }
            last_usage = match (last_usage, usage) {
                (Some(prev), Some(next)) => Some(prev.merge(&next)),
                (Some(prev), None) => Some(prev),
                (None, Some(next)) => Some(next),
                (None, None) => None,
            };
            append_round_text(&mut final_text, &text);
            if calls.is_empty() {
                break;
            }
            // A tool round just started: mark the state so the next
            // iteration's Jev question sees "tool_ran = yes".
            had_tool_results = true;
            // The running indicator now shows what each call actually does
            // (`execute_command cargo test …`), not just the tool name.
            for call in &calls {
                let cid = call.id.as_deref().unwrap_or("");
                let _ = chunk_tx
                    .send(tool_args_marker(
                        cid,
                        &format_call_brief(&call.tool_name, &call.arguments),
                    ))
                    .await;
            }
            // Delta §14.1: deobfuscate placeholders in every tool
            // call's arguments before the tool runs. A model that
            // read `«Credential-abc»` and writes that string back in
            // an `edit`/`write_file` argument gets the raw value
            // substituted here — the model never saw the bytes; the
            // tool gets them. The mutation is on the local `calls`
            // vec, so the assistant message's `tool_calls` field
            // (which stays with the placeholder in the transcript)
            // is unaffected.
            for call in calls.iter_mut() {
                let n = self.deobfuscate_json(&mut call.arguments).await;
                if n > 0 {
                    tracing::debug!(
                        tool = %call.tool_name,
                        count = n,
                        "deobfuscated secret placeholders in tool arguments",
                    );
                }
            }
            let section = self
                .run_tool_calls_with_speculations(
                    &calls,
                    round.holder,
                    Some(chunk_tx),
                    &speculations,
                )
                .await;
            // Delta §9.4: check for a repeated tool round. The
            // corrective is a request-shaped System message; the TUI
            // does not see it (only `messages` is affected), and the
            // next round's provider call is the one that reads it.
            self.maybe_emit_loop_corrective(round.holder, &calls, &section.results, messages)
                .await;
            self.maybe_emit_todo_nudge(round.holder, &calls, messages)
                .await;
            // Each call finished: hand the TUI its completion live (header
            // + summary + wall time) so the "running …" row fills in now,
            // not when the whole loop returns. Markers travel the same
            // channel in call order; the task-end `ToolCompleted` events
            // remain as fallback and are idempotent there.
            for (call, (result, ms)) in calls
                .iter()
                .zip(section.results.iter().zip(section.elapsed_ms.iter()))
            {
                let cid = call.id.as_deref().unwrap_or("");
                let header = format_tool_header(&call.tool_name, &call.arguments);
                let summary = summarize_tool_result(&call.tool_name, result);
                let _ = chunk_tx
                    .send(tool_done_marker(cid, &header, &summary, *ms))
                    .await;
            }
            // Tier 1.5 — record the results we just got into the
            // trace's tool-call records. `add_tool_call` already ran
            // before dispatch; we retroactively attach the summary
            // now that it exists.
            if let Some(mutex) = round.trace
                && let Ok(mut g) = mutex.lock()
            {
                g.attach_results(&section.results, &calls);
            }
            tool_calls.extend(calls);
            tool_results.extend(section.results.clone());
            // Structured transcript slice (design §2 AD-02): the
            // assistant's tool calls + the tool results go into the
            // request messages. The text `pending` string is kept in
            // sync for `apply_steers` and the exhausted-rounds note.
            messages.extend(section.messages.iter().cloned());
            pending.push_str(&format!("\n\n{}", section.prompt_block));
            // Persist the structured slice (AD-02). Same rationale as
            // the collected loop above.
            if !section.messages.is_empty() {
                let mut hist = self.history.write().await;
                let turns = hist.entry(round.holder.to_string()).or_default();
                turns.extend(section.messages.iter().cloned());
                cap_transcript(&mut *turns, MAX_HISTORY_TURNS);
            }
            self.apply_steers(pending, messages, round.holder).await;
            // If we have already produced text this turn, emit a
            // blank-line separator into the chunk stream before the
            // next round. A consumer that prints chunks straight
            // through (the CLI's `kod chat`) would otherwise see the
            // two rounds' text jammed into one sentence. The TUI
            // trims leading blank lines on flush (see
            // `trim_blank_lines`), so this is a no-op for it.
            if !final_text.is_empty() {
                let _ = chunk_tx.send("\n\n".to_string()).await; // kod-round-separator
            }
            // Tool is done, result reinjected — next provider call is pure
            // LLM thinking, not tool execution. Tell the UI to drop the
            // "tool: …" line so a slow model doesn't look like a stuck tool.
            let _ = chunk_tx.send(thinking_marker()).await;
        }
        // Exited on the round cap (empty text + tool calls present).
        // Append the note for the model, and send a visible line down
        // the chunk stream so the user sees why generation stopped
        // short of a final answer.
        if final_text.trim().is_empty() && !tool_calls.is_empty() {
            pending.push_str(TOOL_ROUNDS_EXHAUSTED_NOTE);
            // Same treatment as a steer: the note must be visible to
            // the summary call on the structured path, not just in
            // the text trace.
            messages.push(kod_types::ChatMessage::text(
                kod_types::MessageId::new(),
                kod_types::MessageRole::User,
                TOOL_ROUNDS_EXHAUSTED_NOTE.trim().to_string(),
                time::OffsetDateTime::now_utc(),
            ));
            let _ = chunk_tx
                .send(format!(
                    "\n\n[tool-round limit ({MAX_TOOL_ROUNDS}) reached — summarising progress]\n"
                ))
                .await;
        }
        Ok((final_text, tool_calls, tool_results, last_usage, false))
    }

    /// Open a streaming completion against a fallback endpoint for
    /// the P5.6 mid-stream switch. Returns the provider, its
    /// `ModelRef`, and the boxed stream so `stream_round` can swap
    /// the stream in place without breaking the round.
    pub(crate) async fn fallback_stream_for_off_track(
        &self,
        system_text: &str,
        messages: &[kod_types::ChatMessage],
        definitions: &[ToolDefinition],
        options: &GenerationOptions,
        fallback: &ModelRef,
    ) -> Option<(
        Arc<dyn LlmProvider>,
        ModelRef,
        futures::stream::BoxStream<'static, Result<kod_provider::StreamChunk>>,
    )> {
        let provider = self.resolve_provider_for_model_ref(fallback).await.ok()?;
        let req = self
            .build_grounded_request(
                "",
                system_text,
                messages.to_vec(),
                definitions,
                options,
                fallback,
            )
            .await;
        // Box the stream so it can be returned across the await
        // boundary. The request must be owned by the stream's
        // closure because `stream_completion` borrows it.
        let provider_clone = provider.clone();
        let req_owned = req.clone();
        // Same trick as the primary stream: `async_stream` owns its
        // captures, so the boxed stream is `'static`.
        let stream = Box::pin(async_stream::stream! {
            let inner = provider_clone.stream_completion(&req_owned);
            let mut inner = inner;
            use futures::StreamExt;
            while let Some(item) = inner.next().await {
                yield item;
            }
        })
            as futures::stream::BoxStream<'static, Result<kod_provider::StreamChunk>>;
        Some((provider, fallback.clone(), stream))
    }

    /// One streaming round: forward text live, assemble tool calls from
    ///
    /// Round outcome: text, tool calls, usage, and — new for P5.6 —
    /// a `retry_suggested` flag. `true` means Jev judged the round
    /// off-track and the caller may want to try the next endpoint
    /// with a fresh stream. The text in that case is whatever was
    /// accumulated before the abort; the caller discards it.
    pub(crate) async fn stream_round(
        &self,
        provider: &Arc<dyn LlmProvider>,
        system_text: &str,
        messages: &[kod_types::ChatMessage],
        model_ref: &ModelRef,
        definitions: &[ToolDefinition],
        options: &GenerationOptions,
        chunk_tx: &tokio::sync::mpsc::Sender<String>,
        holder: &str,
        round_trace: Option<&std::sync::Mutex<kod_core_state::trace::TurnTraceBuilder>>,
        fallback: Option<&ModelRef>,
    ) -> Result<StreamRoundOutcome> {
        use futures::StreamExt;
        use std::collections::BTreeMap;

        #[derive(Default)]
        struct Partial {
            id: Option<String>,
            name: Option<String>,
            args: String,
        }

        // Build the structured request the provider will see. The
        // streaming variant of `complete` is `stream_completion`; its
        // working default collects the reply and replays it, so a
        // provider that has not overridden it still produces chunks in
        // the right order. Both concrete providers in this workspace
        // override it with real SSE.
        let req = self
            .build_grounded_request(
                holder,
                system_text,
                messages.to_vec(),
                definitions,
                options,
                model_ref,
            )
            .await;
        // P5.6 — wrap the concrete stream in an `async_stream` that
        // owns its provider and request. `stream_completion` borrows
        // both, so its return type carries a lifetime; the wrapper
        // collects everything into a `'static` box we can swap
        // mid-round. The cost is one clone of the provider `Arc` and
        // the request per round — negligible next to the model call.
        let mut stream: futures::stream::BoxStream<'static, Result<kod_provider::StreamChunk>> = {
            let provider_owned = provider.clone();
            let req_owned = req.clone();
            Box::pin(async_stream::stream! {
                let inner = provider_owned.stream_completion(&req_owned);
                let mut inner = inner;
                use futures::StreamExt;
                while let Some(item) = inner.next().await {
                    yield item;
                }
            })
        };
        let mut text = String::new();
        let mut stop_reason_out: Option<String> = None;
        let mut partials: BTreeMap<usize, Partial> = BTreeMap::new();
        // Delta §10: speculative reads. Keyed by tool-call index; the
        // value is a JoinHandle that resolves to a completed read.
        // Admitted lazily as a `read_file` call's `path` argument
        // completes mid-stream, so the read overlaps the provider's
        // remaining generation tail.
        let mut speculation_handles: BTreeMap<
            usize,
            tokio::task::JoinHandle<Option<kod_core_tools::speculation::SpeculativeRead>>,
        > = BTreeMap::new();
        // Only speculate when the caller has not disabled it. The
        // engine's `speculative_reads` flag defaults on: the design's
        // cost analysis is "one wasted read in the worst case".
        let speculate_enabled = *self.speculative_reads.read().await;
        let speculate_working_dir = self
            .working_dir_for(if holder.is_empty() { "session" } else { holder })
            .await;
        let mut last_usage: Option<kod_provider::TokenUsage> = None;
        let mut chunk_count: usize = 0;
        let mut retry_suggested = false;
        // H-E11: an idle-chunk deadline. The pre-fix loop
        // (`while let Some(item) = stream.next().await`) had no
        // bound at all: a wedged SSE connection hung the whole turn
        // (and, since it is per-transcript-key, every subsequent
        // prompt on that key). The timeout is per-chunk, not
        // per-stream — a slow-but-alive connection that sends
        // something every few seconds never trips it.
        // A reasoning model produces no output while it thinks, and
        // a `Max`-effort request can be legitimately silent for
        // minutes. The base is 120 s — correct for a fast model — and
        // the effort scales it: 240 s at `High`, 360 s at `Xhigh`,
        // 480 s at `Max`. An unset effort is `Medium`, whose
        // multiplier is 1.0, so every existing caller's timeout is
        // unchanged.
        let stream_idle_timeout = kod_provider::effort::scaled_idle_timeout(
            std::time::Duration::from_secs(120),
            options.effort.unwrap_or_default(),
        );
        let mut stream_error: Option<kod_error::KodError> = None;
        loop {
            let next = match tokio::time::timeout(stream_idle_timeout, stream.next()).await {
                Ok(Some(item)) => item,
                Ok(None) => break,
                Err(_) => {
                    stream_error = Some(kod_error::KodError::ProviderTimeout {
                        timeout_ms: stream_idle_timeout.as_millis() as u64,
                    });
                    break;
                }
            };
            let item = match next {
                Ok(v) => v,
                Err(e) => {
                    // H-E11: preserve what was assembled. The pre-fix
                    // `item?` discarded every partial and the
                    // streamed text on the first hard error, so the
                    // transcript showed a user turn with no
                    // assistant turn after a hard failure.
                    stream_error = Some(e);
                    break;
                }
            };
            match item {
                StreamChunk::Text(t) => {
                    text.push_str(&t);
                    // Delta §14.3: TTSR rules match the streamed prose.
                    // A firing rule with `interrupt` aborts the stream
                    // so the caller injects the correction and retries;
                    // a non-interrupting hit is logged.
                    // T3-C7: compute the interrupt under the lock, then
                    // drop the write guard before any send. Holding the
                    // TTSR write lock across `chunk_tx.send().await`
                    // parks the whole TTSR subsystem when a slow
                    // consumer fills the channel.
                    let interrupt = {
                        let mut engine = self.ttsr.write().await;
                        let fired = engine.observe_text(&t);
                        if !fired.is_empty() {
                            for f in &fired {
                                tracing::debug!(
                                    rule = %f.id,
                                    interrupt = f.interrupt,
                                    "ttsr rule fired",
                                );
                            }
                        }
                        fired.iter().find(|f| f.interrupt).cloned()
                    };
                    if let Some(f) = interrupt {
                        let _ = chunk_tx.send(t).await;
                        stream_error = Some(KodError::InvalidState(format!(
                            "ttsr rule `{}` fired: {}",
                            f.id, f.correction,
                        )));
                        break;
                    }
                    let _ = chunk_tx.send(t).await;
                    chunk_count += 1;
                    // Early-termination check (P1.2). The
                    // character floor and the sentence
                    // requirement are enforced inside the helper;
                    // the chunk counter here just gates the call
                    // rate.
                    if chunk_count.is_multiple_of(EARLY_TERM_CHECK_EVERY_CHUNKS)
                        && text.len() >= EARLY_TERM_MIN_CHARS
                    {
                        match self.should_early_terminate(holder, &text).await {
                            EarlyTermination::Complete => break,
                            EarlyTermination::OffTrack => {
                                // P5.6 — try a mid-stream switch to
                                // the fallback endpoint. If one is
                                // available, keep the accumulated
                                // text and continue reading from the
                                // new stream; the consumer sees one
                                // uninterrupted reply.
                                if let Some(fb) = fallback
                                    && let Some((_prov, _m, new_stream)) = self
                                        .fallback_stream_for_off_track(
                                            system_text,
                                            messages,
                                            definitions,
                                            options,
                                            fb,
                                        )
                                        .await
                                {
                                    stream = new_stream;
                                    // Reset the counter so the new
                                    // stream gets a fresh window
                                    // before the next check.
                                    chunk_count = 0;
                                    continue;
                                }
                                // No fallback available: signal the
                                // outer chain loop to retry against
                                // the next endpoint. Pre-P5.6
                                // behaviour.
                                retry_suggested = true;
                                break;
                            }
                            EarlyTermination::None => {}
                        }
                    }
                }
                StreamChunk::ToolCallStart { index, id, name } => {
                    let entry = partials.entry(index).or_default();
                    if entry.id.is_none() {
                        entry.id = id;
                    }
                    if entry.name.is_none() {
                        entry.name = Some(name.clone());
                        let cid = entry.id.as_deref().unwrap_or("");
                        let _ = chunk_tx.send(tool_start_marker(cid, &name)).await;
                    }
                }
                StreamChunk::ToolCallDelta { index, arguments } => {
                    let entry = partials.entry(index).or_default();
                    entry.args.push_str(&arguments);
                    // Delta §14.3: TTSR rules also match a tool call's
                    // streamed arguments. This runs before the
                    // speculation block below, which may `continue` and
                    // skip the rest of the arm.
                    if let Some(name) = entry.name.clone() {
                        let partial_path =
                            kod_core_tools::speculation::extract_path_from_partial(&entry.args);
                        let mut engine = self.ttsr.write().await;
                        let fired =
                            engine.observe_tool(&name, partial_path.as_deref(), &entry.args);
                        let interrupt = fired.iter().find(|f| f.interrupt).cloned();
                        for f in &fired {
                            tracing::debug!(
                                rule = %f.id,
                                tool = %name,
                                interrupt = f.interrupt,
                                "ttsr tool rule fired",
                            );
                        }
                        if let Some(f) = interrupt {
                            stream_error = Some(KodError::InvalidState(format!(
                                "ttsr rule `{}` fired on `{name}`: {}",
                                f.id, f.correction,
                            )));
                            break;
                        }
                    }
                    // Delta §10: as soon as the args carry a complete
                    // `"path":"..."` for a `read_file` call and no
                    // speculation has been admitted for this index
                    // yet, spawn one. The `path` usually streams
                    // before the JSON closes, so this fires well
                    // before the round ends and the read overlaps the
                    // remaining generation.
                    if speculate_enabled
                        && entry.name.as_deref() == Some("read_file")
                        && !speculation_handles.contains_key(&index)
                        && let Some(rel) =
                            kod_core_tools::speculation::extract_path_from_partial(&entry.args)
                    {
                        // Resolve relative to the transcript's working
                        // dir, matching what `read_file` will do. A
                        // path that escapes the root is not
                        // speculatable: resolve_path will refuse it
                        // and the read would fail anyway.
                        let abs = if std::path::Path::new(&rel).is_absolute() {
                            std::path::PathBuf::from(&rel)
                        } else {
                            speculate_working_dir.join(&rel)
                        };
                        if !abs.exists() {
                            // Not a candidate — the ordinary tool call
                            // will produce the "no such file" error.
                            continue;
                        }
                        let handle = tokio::spawn(async move {
                            match kod_core_tools::speculation::read_with_evidence(&abs) {
                                Ok((text, evidence)) => Some(kod_core_tools::speculation::SpeculativeRead {
                                    path: abs,
                                    text,
                                    evidence,
                                }),
                                Err(_) => None,
                            }
                        });
                        speculation_handles.insert(index, handle);
                    }
                }
                StreamChunk::Usage(usage) => {
                    last_usage = Some(usage);
                }
                StreamChunk::StopReason(reason) => {
                    // H-P6: captured for the caller; the engine has no
                    // policy on truncation yet (that is a follow-up).
                    // Delta §9.11: also threaded out so the run
                    // collector's stop-reason histogram is populated.
                    tracing::debug!(reason = %reason, "provider stop_reason");
                    stop_reason_out = Some(reason);
                }
                StreamChunk::Done => break,
            }
        }
        // Tier 1.4 — record this round's usage into the trace before
        // returning. No-op when no writer is installed.
        if let Some(mutex) = round_trace {
            let (prompt, completion) = match last_usage.as_ref() {
                Some(u) => (u.prompt_tokens, u.completion_tokens),
                None => (0_usize, 0_usize),
            };
            if let Ok(mut g) = mutex.lock() {
                g.add_usage(prompt, completion, None, 0.0);
            }
        }

        let mut calls = Vec::with_capacity(partials.len());
        // Delta §10: join every speculative read's handle. The calls
        // vec and the speculations vec are indexed in parallel — the
        // consumer (`run_tool_calls`) validates the speculation
        // against the live file before using it and falls back to a
        // fresh read on any mismatch.
        let mut speculations: Vec<Option<kod_core_tools::speculation::SpeculativeRead>> =
            Vec::with_capacity(partials.len());
        for (index, p) in partials {
            let Some(name) = p.name else { continue };
            // T3-H17: an unparseable argument blob is a real error, not a
            // JSON string masquerading as arguments. Record it so the
            // dispatch can report "malformed arguments" rather than a
            // generic "missing argument" when a tool reads `path`.
            let arguments: serde_json::Value = match serde_json::from_str(&p.args) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(
                        tool = %name,
                        error = %e,
                        raw = %p.args.chars().take(120).collect::<String>(),
                        "streamed tool call had malformed arguments",
                    );
                    serde_json::Value::Null
                }
            };
            calls.push(ToolCall {
                id: p.id,
                tool_name: name,
                arguments,
            });
            let spec = match speculation_handles.remove(&index) {
                Some(handle) => handle.await.ok().flatten(),
                None => None,
            };
            speculations.push(spec);
        }
        // H-E11: a mid-stream error or an idle timeout still returns
        // everything the loop managed to assemble — the caller sees
        // partial text and any complete tool calls, and the
        // `Result` carries the error so the caller can decide
        // whether to surface it.
        // M-13: hand back whatever the loop assembled, plus the
        // error. The caller can execute complete calls and surface
        // the error; pre-fix the error discarded both.
        Ok(StreamRoundOutcome {
            text,
            calls,
            usage: last_usage,
            retry_suggested,
            speculations,
            stop_reason: stop_reason_out,
            partial_error: stream_error,
        })
    }

    /// Stream a plain-text summary (tools already ran): forwards chunks live.
    pub(crate) async fn stream_summary(
        &self,
        provider: &Arc<dyn LlmProvider>,
        pending: &str,
        options: &GenerationOptions,
        chunk_tx: &tokio::sync::mpsc::Sender<String>,
    ) -> Result<String> {
        use futures::StreamExt;
        let mut stream = provider.stream(pending, options);
        let mut text = String::new();
        // M-17: same per-chunk idle timeout `stream_round` uses (H-E11).
        // Without it a wedged summary stream hangs the transcript key
        // indefinitely.
        let idle = std::time::Duration::from_secs(60);
        loop {
            let next = tokio::time::timeout(idle, stream.next()).await;
            let item = match next {
                Ok(Some(item)) => item,
                Ok(None) => break,
                Err(_) => {
                    tracing::warn!("summary stream idle timeout; using partial text");
                    break;
                }
            };
            match item {
                Ok(StreamChunk::Text(t)) => {
                    text.push_str(&t);
                    let _ = chunk_tx.send(t).await;
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(error = %e, "summary stream errored; using partial text");
                    break; // keep `text` — do not discard it
                }
            }
        }
        Ok(text)
    }

    /// Build a grounded `CompletionRequest` from the router plan and
    /// the transcript (design §2 AD-01, AD-16).
    ///
    /// The split:
    ///
    /// - **system**: the router's `PromptPlan::system` segments
    ///   concatenated in order, with the environment + tool inventory
    ///   grounding appended as a final volatile segment. The
    ///   cacheable / volatile ordering is preserved by rendering the
    ///   plan to text first and grounding the result — a provider with
    ///   explicit cache support (Anthropic) is given the whole string
    ///   as one segment; the split between cacheable and volatile is
    ///   not needed for correctness on either wire today, only for
    ///   cache *placement*. A follow-up can pass the segments through
    ///   as separate `SystemPrompt::segments` to place the breakpoint
    ///   exactly, and the design notes the shape.
    ///
    /// - **messages**: the transcript as structured `ChatMessage`s
    ///   (`self.history[key]` already holds the user's turn by the time
    ///   this runs; the assistant's turns and the tool rounds are
    ///   appended by the loops).
    ///
    /// The `model` field carries the resolved endpoint, so a provider
    /// that is asked to switch mid-stream can honour the request's
    /// choice rather than the one baked in at construction.
    /// # P0 marker gate
    ///
    /// This function is `async` because it consults the tool-filter
    /// state for a pending marker suppression. The suppression is
    /// armed by `filter_tool_definitions_with_hysteresis` when a
    /// commit changed the enabled tool set, and consumed here on
    /// the next call so exactly one request per prefix change skips
    /// the transcript cache breakpoint.
    /// Fingerprint the tool surface for `key` and journal a change.
    ///
    /// Split out so `build_grounded_request` stays focused. The hash
    /// is over the concatenation of `name`, `description`, and the
    /// serialized `parameters_schema` per tool, in registry order.
    /// That order is sorted (see `registry.rs`) so a re-registration
    /// of the same set produces the same fingerprint.
    pub(crate) async fn note_tool_surface_fingerprint(&self, key: &str, definitions: &[ToolDefinition]) {
        // T3-C1: FNV-1a instead of `DefaultHasher::new()`. The latter
        // is seeded randomly per call, so the same tool surface
        // produced a different fingerprint on every prompt — the
        // cache journal recorded a spurious `ToolSurfaceChanged` on
        // every turn and the cache-invalidation check never fired.
        let mut hasher = Fnv1aHasher::new();
        for d in definitions {
            use std::hash::Hash;
            d.name.hash(&mut hasher);
            d.description.hash(&mut hasher);
            d.parameters_schema.to_string().hash(&mut hasher);
        }
        let fingerprint = std::hash::Hasher::finish(&hasher);

        let mut guard = self.tool_surface_fingerprint.write().await;
        let previous = guard.insert(key.to_string(), fingerprint);
        drop(guard);

        if let Some(prev) = previous {
            if prev != fingerprint {
                // Late MCP registration is the expected cause. Anything
                // else is worth a look at the journal.
                kod_core_state::cache_journal::record(
                    kod_core_state::cache_journal::InvalidationCause::ToolSurfaceChanged {
                        previous_fingerprint: prev,
                        current_fingerprint: fingerprint,
                        reason: format!("{} definitions on this request", definitions.len()),
                    },
                );
            }
        }
    }

    /// Delta §4.5: the inline-imaging pass.
    ///
    /// Walk the outgoing messages and, for each large *text* tool
    /// result with a vision provider in play, rasterize its body
    /// into a PNG and attach it to the message's metadata. The wire
    /// layer emits the image; the text stays in `content` for the
    /// local transcript and any fallback.
    pub(crate) async fn inline_image_tool_results(&self, messages: &mut [kod_types::ChatMessage]) {
        let vision = match self.current_provider().await {
            Some(p) => p.capabilities().vision,
            None => false,
        };
        if !vision {
            return;
        }

        // The freshest tool result is the working set; never image
        // it.
        let last_tool = messages
            .iter()
            .rposition(|m| m.role == kod_types::MessageRole::Tool);

        for (i, m) in messages.iter_mut().enumerate() {
            if m.role != kod_types::MessageRole::Tool {
                continue;
            }
            if Some(i) == last_tool {
                continue;
            }
            if m.content.starts_with("Error:") || m.content.starts_with("error:") {
                continue;
            }
            if m.metadata.image.is_some() {
                continue;
            }
            let tokens = (m.content.len() / 4) as u64;
            if tokens < MIN_INLINE_IMAGE_TOKENS {
                continue;
            }
            let frame_est = crate::compaction_dispatcher::FRAME_TOKEN_ESTIMATE;
            let ratio = frame_est as f64 / tokens.max(1) as f64;
            if ratio >= crate::compaction_dispatcher::SAVINGS_MARGIN {
                continue;
            }

            let cache_key = format!(
                "{}:{}",
                m.tool_call_id.as_deref().unwrap_or(""),
                simple_hash(&m.content),
            );
            let cached = {
                let g = self.image_render_cache.read().await;
                g.get(&cache_key).cloned()
            };
            let frame = match cached {
                Some(f) => f,
                None => match kod_core_quality::snapcompact::rasterize_to_png(&m.content) {
                    Ok(png) => {
                        let f = kod_types::RasterizedImage {
                            png_base64: kod_core_quality::snapcompact::base64_encode(&png),
                            media_type: "image/png".to_string(),
                            source_lines: m.content.lines().count(),
                        };
                        self.image_render_cache
                            .write()
                            .await
                            .insert(cache_key, f.clone());
                        f
                    }
                    Err(_) => continue,
                },
            };
            m.metadata.image = Some(frame);
        }
    }

    pub(crate) async fn build_grounded_request(
        &self,
        key: &str,
        system_text: &str,
        messages: Vec<kod_types::ChatMessage>,
        definitions: &[ToolDefinition],
        options: &GenerationOptions,
        model: &ModelRef,
    ) -> CompletionRequest {
        // The router's rendered plan ends with a transcript section
        // (`## Conversation so far`) followed by the user's request
        // (`## User Request`). Both are already passed as `messages`
        // — duplicating them inside the system prompt wastes tokens
        // and confuses a model that sees the same turn twice.
        let head = strip_conversation_tail(system_text);

        // Split the head at the router's own marker. Everything before
        // `## Volatile suffix` is the byte-stable cacheable prefix
        // (Identity + Repository map); everything from it on is the
        // volatile tail (Environment, tool inventory, skills, memory).
        // A provider with explicit cache support places a breakpoint at
        // the last cacheable segment; this two-segment shape is what
        // makes that breakpoint meaningful.
        const VOLATILE_MARKER: &str = "## Volatile suffix";
        let (cacheable, volatile_tail) = match head.find(VOLATILE_MARKER) {
            Some(i) => (head[..i].trim_end().to_string(), head[i..].to_string()),
            None => {
                // No marker (custom-built prompt): the whole thing is
                // volatile. Honest degradation — a caller that lost
                // the convention loses the cache benefit, not
                // correctness.
                (String::new(), head)
            }
        };

        // The environment + tool inventory grounding is appended to the
        // volatile segment — it depends on the current tool set and on
        // the working directory, so it is never cacheable. `ground_prompt`
        // appends a leading blank line + `## Environment` block, so
        // passing an empty tail still produces a valid segment.
        let grounded_volatile = self.ground_prompt(key, volatile_tail, definitions);

        // Delta §12.7: the session's mental-model block, if any. It
        // is a cacheable segment because it is frozen for the session —
        // re-rendering it mid-turn would move the bytes after it and
        // invalidate the provider's prefix cache. Empty when no model
        // is seeded, so the default session's prompt is byte-identical
        // to the pre-§12.7 shape.
        let mental_block = self.mental_models.read().await.render_block();
        let mut system = SystemPrompt::new();
        if !mental_block.is_empty() {
            system = system.with(mental_block, true);
        }
        if !cacheable.is_empty() {
            system = system.with(cacheable, true);
        }
        system = system.with(grounded_volatile, false);
        // P0-c/P1-a: fingerprint the tool surface and journal any
        // change. The tools array is part of the cached prefix; a
        // change here invalidates it at every endpoint. Recording the
        // cause turns a silent miss into a debuggable event. Hash is
        // over the serialized definitions in the order the registry
        // returns them (which is byte-stable per `registry.rs`'s sort).
        self.note_tool_surface_fingerprint(key, definitions).await;

        // Delta §6: demote Discoverable tools out of the array the
        // provider sees. They stay registered and reachable through
        // `read xd://<tool>` / `write xd://<tool>`; the schema just
        // stops costing prompt budget on every request. A tool whose
        // load mode is Essential (the default) is unaffected.
        let definitions: Vec<ToolDefinition> = definitions
            .iter()
            .filter(|d| !d.load_mode.is_discoverable())
            .cloned()
            .collect();

        // Delta §14.1: obfuscate registered secrets in every string
        // about to reach the provider.
        //
        // The gate is *the vault's presence*, not
        // `[security.redact] in_prompt`. The two are different
        // mechanisms with different goals: `in_prompt` runs the
        // one-way [`kod_types::redact::Redactor`] and replaces a
        // secret with `[REDACTED:...]`, which the model cannot
        // round-trip; the vault replaces a secret with a placeholder
        // the model can work with and the engine reverses. A user
        // who installed a vault wants the reversible form; a user
        // who did not gets raw bytes and the pre-§14.1 shape.
        let (system, messages) = {
            let vault = self.secret_vault.read().await.clone();
            match vault {
                Some(v) if !v.is_empty() => {
                    let system = SystemPrompt {
                        segments: system
                            .segments
                            .into_iter()
                            .map(|s| SystemSegment {
                                text: v.obfuscate(&s.text),
                                cacheable: s.cacheable,
                            })
                            .collect(),
                    };
                    let messages: Vec<kod_types::ChatMessage> = messages
                        .into_iter()
                        .map(|mut m| {
                            m.content = v.obfuscate(&m.content);
                            m
                        })
                        .collect();
                    (system, messages)
                }
                _ => (system, messages),
            }
        };

        let mut req = CompletionRequest {
            system,
            messages,
            tools: definitions.to_vec(),
            options: options.clone(),
            model: model.clone(),
            // P0 cache control: normally the transcript carries a
            // cache breakpoint so a long session reads its context
            // back at the cache rate. When the tool filter just
            // changed the enabled set, the prefix is about to churn,
            // so this one request skips the marker and avoids paying
            // Anthropic's 1.25x cache-write premium for a prefix that
            // will not survive the next round.
            cache_transcript: !self.consume_marker_suppression(key).await,
            // Delta §4.4: attach any stored provider-native compaction
            // block for this transcript. The provider that
            // understands the block (Anthropic) prepends it to the
            // first user message; every other provider ignores the
            // field.
            native_compaction_block: self.native_compaction_blocks.read().await.get(key).cloned(),
            // Delta §4.5: attach any stored image frames. The
            // Anthropic wire emits them as image content blocks on
            // the first user message; other providers ignore them.
            image_frames: self
                .image_frames
                .read()
                .await
                .get(key)
                .cloned()
                .unwrap_or_default(),
            // Tab-bridge affinity: stateful tab providers route this
            // request into the transcript's tab via the OpenAI `user`
            // field. Stateless providers ignore it.
            session_id: Some(self.session_id_for_holder(key).to_prefixed_string()),
        };
        // Delta §9.10: trim the frame vec to the provider's per-request
        // budget before any provider serializes it. A dropped frame is
        // counted by the report; today the engine logs at debug because
        // the drop is expected (an old frame that no longer fits) and a
        // per-turn warn would be noise.
        let report =
            req.apply_image_budget(&kod_provider::image_budget::ImageBudgetPolicy::default());
        if report.any_dropped() {
            tracing::debug!(
                holder = key,
                dropped_undecodable = report.dropped_undecodable,
                dropped_oversize = report.dropped_oversize,
                dropped_over_cap = report.dropped_over_cap,
                "image budget dropped frames before serialization",
            );
        }
        req
    }

    /// Append the environment + tool inventory grounding to a router prompt.
    /// Look up the trust level of a tool by name (Tier 1.1).
    pub(crate) async fn tool_trust_level(&self, name: &str) -> Option<kod_types::trust::TrustLevel> {
        let defs = self.tools.get_definitions().await;
        defs.into_iter()
            .find(|d| d.name == name)
            .map(|d| d.trust_level)
    }

    pub(crate) fn ground_prompt(
        &self,
        key: &str,
        mut prompt: String,
        definitions: &[ToolDefinition],
    ) -> String {
        prompt.push_str(&format!(
            "\n## Environment\n\n- Working directory: {}\n- OS: {}\n",
            self.working_dir.display(),
            std::env::consts::OS
        ));
        // Tier 2.1 — if a plan exists for the default transcript,
        // prepend it to the prompt. The plan is a stable target the
        // model can consult on every round.
        //
        // `ground_prompt` is synchronous, so we peek at the map with
        // a `try_read`; a rare miss is fine (the plan appears on the
        // next round).
        if let Ok(g) = self.plans.try_read()
            && let Some(plan) = g.get(key)
        {
            prompt.push_str("\n\n");
            prompt.push_str(&plan.render_prompt_block());
        }
        // Tier 3.4 — recent durable decisions. Bounded to 20 so the
        // block stays small even in a long session.
        if let Ok(g) = self.decision_logs.try_read()
            && let Some(log) = g.get(key)
            && !log.entries.is_empty()
        {
            prompt.push_str("\n\n");
            prompt.push_str(&log.render_prompt_block(20));
        }

        // Tier 3.5 — shared blackboard, when this transcript is
        // subscribed (i.e. an agent in a swarm). Bounded to 30
        // entries and 3000 chars so a chatty swarm cannot crowd out
        // the actual task.
        if let Ok(g) = self.blackboard_viewers.try_read()
            && g.contains(key)
        {
            let block = self.blackboard.render_prompt_block("team", 30, 3000);
            if !block.is_empty() {
                prompt.push_str("\n\n");
                prompt.push_str(&block);
            }
        }

        if !definitions.is_empty() {
            let names: Vec<String> = definitions
                .iter()
                .map(|d| format!("- {}: {}", d.name, d.description))
                .collect();
            prompt.push_str(&format!(
                "\n## Tool use\n\nYou have these tools (function calls, rooted at the working directory above):\n{}\nCall them when you need facts from this machine instead of guessing. Tool outputs return as `## Tool results` blocks — then answer the user.\n",
                names.join("\n")
            ));
            // Tier 1.1 — the trust invariant. Injected whenever the
            // prompt carries a tool inventory, since that is the
            // precondition for a tool result block later in the turn.
            // The wording is load-bearing; see TRUST_INVARIANT.
            prompt.push_str("\n## Trust boundary\n\n");
            prompt.push_str(kod_types::trust::TRUST_INVARIANT);
            prompt.push('\n');
        }
        prompt
    }

    /// Delta §11.7: offer the todo tool at the start of a task.
    ///
    /// Injected as a System message into the transcript so the first
    /// round's request carries it. Offered at most once per
    /// transcript, and skipped for a question (`?`) or exclamation
    /// (`!`) prompt, or when todos already exist.
    pub(crate) async fn maybe_offer_todo_prelude(&self, key: &str, input: &str) {
        // Only the first turn of a *task*. A transcript with prior
        // messages is a resumed session or a mid-task turn — the
        // model already has context and does not need the workflow
        // explained. This also keeps the prelude out of a seeded or
        // replayed transcript.
        let has_history = {
            let h = self.history.read().await;
            h.get(key).map(|t| !t.is_empty()).unwrap_or(false)
        };
        if has_history {
            return;
        }
        let has_todos = !self.todo_list.read().await.is_empty();
        let mut trackers = self.todo_trackers.write().await;
        let tracker = trackers
            .entry(key.to_string())
            .or_insert_with(kod_tools::todo_tracker::TodoTracker::new);
        if !tracker.should_offer_prelude(input, has_todos) {
            return;
        }
        tracker.note_prelude_offered();
        drop(trackers);
        let mut hist = self.history.write().await;
        hist.entry(key.to_string())
            .or_default()
            .push(kod_types::ChatMessage::text(
                kod_types::MessageId::new(),
                kod_types::MessageRole::System,
                "[todo prelude] For a multi-step task, keep a todo list \
                 with the `todo` tool: one item in progress at a time, \
                 mark items done as you finish them, and add items you \
                 discover as you go. Skip the list for a single-step ask.",
                time::OffsetDateTime::now_utc(),
            ));
    }

    /// Delta §11.7: remind the model of open todos when it stops.
    ///
    /// The reminder is appended to the transcript so the next turn
    /// sees it. It is not a continuation: the engine does not re-enter
    /// the loop on its own — the caller decides whether to keep going.
    /// Skipped when the final line is a question (the model is asking,
    /// not stopping) or a background job will wake it.
    pub(crate) async fn maybe_emit_todo_completion_reminder(&self, key: &str, final_text: &str) {
        let incomplete = kod_tools::todo::in_progress_todo(&self.todo_list).is_some();
        let last_line_is_question = final_text
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .map(|l| l.trim_end().ends_with('?'))
            .unwrap_or(false);
        let async_wakes_pending = self.async_delivery.lock().queued_for(key) > 0;
        let mut trackers = self.todo_trackers.write().await;
        let tracker = trackers
            .entry(key.to_string())
            .or_insert_with(kod_tools::todo_tracker::TodoTracker::new);
        if !tracker.completion_reminder_due(incomplete, last_line_is_question, async_wakes_pending)
        {
            return;
        }
        tracker.note_completion_reminder();
        drop(trackers);
        let mut hist = self.history.write().await;
        hist.entry(key.to_string())
            .or_default()
            .push(kod_types::ChatMessage::text(
                kod_types::MessageId::new(),
                kod_types::MessageRole::System,
                "[todo reminder] The todo list still has an item in \
                 progress. If the task is finished, mark it done; \
                 otherwise continue.",
                time::OffsetDateTime::now_utc(),
            ));
    }

    /// Delta §11.7: record a tool round and, when the tracker says a
    /// mid-run reconcile nudge is due, inject it as a System message.
    ///
    /// Mirrors `maybe_emit_loop_corrective`'s shape: per-transcript
    /// state, a System-message injection, no effect on the round's
    /// results.
    pub(crate) async fn maybe_emit_todo_nudge(
        &self,
        key: &str,
        calls: &[kod_types::ToolCall],
        messages: &mut Vec<kod_types::ChatMessage>,
    ) {
        let mut trackers = self.todo_trackers.write().await;
        let tracker = trackers
            .entry(key.to_string())
            .or_insert_with(kod_tools::todo_tracker::TodoTracker::new);
        for call in calls {
            let is_todo = call.tool_name == "todo";
            let is_mutating = matches!(
                call.tool_name.as_str(),
                "write_file" | "patch_file" | "execute_command",
            );
            tracker.observe_tool(is_mutating, is_todo);
        }
        let due = tracker.take_mid_run_nudge();
        drop(trackers);
        if !due {
            return;
        }
        let body = "[todo reconcile] You have run many mutating tools                     without updating the todo list. Update it now: mark                     finished items done, add anything you discovered, and                     keep exactly one item in progress.";
        messages.push(kod_types::ChatMessage::text(
            kod_types::MessageId::new(),
            kod_types::MessageRole::System,
            body,
            time::OffsetDateTime::now_utc(),
        ));
    }
}

/// T3-C1: a deterministic 64-bit FNV-1a hasher. `DefaultHasher::new()`
/// reseeds per call — using it as a fingerprint source means two calls
/// for the identical input produce different output, so any
/// change-detection built on top of it always reports "changed".
struct Fnv1aHasher(u64);

impl Fnv1aHasher {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    fn new() -> Self {
        Self(Self::OFFSET)
    }
}

impl std::hash::Hasher for Fnv1aHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 ^= *b as u64;
            self.0 = self.0.wrapping_mul(Self::PRIME);
        }
    }
}
