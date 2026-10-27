use super::*;

impl KodEngine {
    /// Keyed variant of the transcript + memory writer.
    pub(crate) async fn remember_turn_for(&self, key: &str, user: bool, text: &str) {
        self.record_turn_for(key, user, text).await;
        // Short-term memory is intentionally shared across transcripts:
        // it is the retrieval-side working set and the retrieve path
        // filters by input words, so an agent asking about "SQL schema"
        // will not surface a sibling agent's turn about "HTTP handler".
        let _ = self.router.store_short_term(text).await;
    }

    /// Test-only wrapper for the default transcript.
    #[cfg(test)]
    pub(crate) async fn remember_turn(&self, user: bool, text: &str) {
        self.remember_turn_for(DEFAULT_TRANSCRIPT_KEY, user, text)
            .await
    }

    /// Test-only wrapper for the default transcript.
    #[cfg(test)]
    pub(crate) async fn record_turn(&self, user: bool, text: &str) {
        self.record_turn_for(DEFAULT_TRANSCRIPT_KEY, user, text)
            .await
    }

    /// Test-only wrapper for the default transcript.
    #[cfg(test)]
    pub(crate) async fn render_history(&self) -> String {
        self.render_history_for(DEFAULT_TRANSCRIPT_KEY).await
    }

    /// Keyed turn recorder. Truncates long texts, keeps only the
    /// most recent [`MAX_HISTORY_TURNS`] turns for `key`. Uses
    /// [`truncate_chars`] rather than a raw slice — the byte-slice
    /// version panicked on non-ASCII text that crossed the cap.
    pub(crate) async fn record_turn_for(&self, key: &str, user: bool, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        let short =
            kod_types::strutil::truncate_with_ellipsis(text, MAX_TURN_CHARS, "… [truncated]");
        let role = if user {
            kod_types::MessageRole::User
        } else {
            kod_types::MessageRole::Assistant
        };
        let message = kod_types::ChatMessage::text(
            kod_types::MessageId::new(),
            role,
            short,
            time::OffsetDateTime::now_utc(),
        );
        let mut history = self.history.write().await;
        let turns = history.entry(key.to_string()).or_default();
        turns.push(message);
        cap_transcript(turns, MAX_HISTORY_TURNS);
        // Char-budget trim: D1 moved the transcript from a rendered
        // text section to a structured `messages` field the provider
        // sees verbatim; without a cap here the budget was ignored.
        let budget = self.history_budget();
        let mut total: usize = 0;
        let mut cutoff = turns.len();
        for i in (0..turns.len()).rev() {
            let line_len = turns[i].render_text().len() + 1;
            if total + line_len > budget {
                break;
            }
            total += line_len;
            cutoff = i;
        }
        for i in (0..cutoff).rev() {
            if turns[i].metadata.pinned {
                cutoff = i;
            }
        }
        if cutoff > 0 {
            // BUG-C-01: never leave a surviving `Tool` row whose
            // owning assistant (with the matching `tool_call_id`) was
            // dropped — providers reject dangling tool results. Extend
            // the drop through any orphaned Tool rows at the new front.
            while cutoff < turns.len() && turns[cutoff].role == kod_types::MessageRole::Tool {
                cutoff += 1;
            }
            turns.drain(..cutoff);
        }
    }

    /// Render the transcript for `key`, oldest-first, dropping the
    /// newest-over-budget entries per the current history budget (see
    /// [`KodEngine::set_history_budget`]).
    pub(crate) async fn render_history_for(&self, key: &str) -> String {
        let history = self.history.read().await;
        let Some(turns) = history.get(key) else {
            return "(start of conversation)".to_string();
        };
        if turns.is_empty() {
            return "(start of conversation)".to_string();
        }
        let budget = self.history_budget();

        // P2: route the FIFO budget walk through the fidelity
        // pipeline in `context_engine`. On this first landing the
        // scorer is configured so every turn scores `Full`; the
        // pipeline output is byte-identical to the previous inline
        // walk, and the characterization tests in
        // `tests/characterization_history.rs` plus the three
        // `render_history_*` unit tests in this module prove the
        // swap. A follow-up narrows `with_tail` so old turns can
        // actually drop below `Full`.
        //
        // P2: the query is the current user turn's text, so the
        // scorer's relevance term has something to work with on
        // turns older than the recency tail. `current_request` is
        // None when a caller renders history without an active turn
        // (unit tests, `/debug` surfaces); in that case the empty
        // query makes every out-of-tail turn score by recency alone,
        // which is the previous FIFO-equivalent behavior.
        let query_text = self.current_request(key).await.unwrap_or_default();
        let query = kod_core_routing::context_engine::Query::from_text(&query_text);
        let scorer = kod_core_routing::context_engine::LexicalScorer::new().with_tail(10); // P2: recent-10 stay Full; older score by relevance

        let mut cache_guard = self.fidelity_cache.write().await;
        let cache = cache_guard
            .entry(key.to_string())
            .or_insert_with(kod_core_routing::context_engine::FidelityCache::new);

        let (out, consult) = kod_core_routing::context_engine::render_scored(
            turns, query, &scorer, cache, budget,
            true, // skip tool rows and empty tool-call assistants
        );

        // Drop cache entries for turns that no longer exist in this
        // transcript key (after a compact or a clear).
        cache.retain_ids(&consult);

        out
    }

    /// The prompt the provider received on the most recent
    /// `process*` call on the default transcript.
    pub async fn last_prompt(&self) -> Option<String> {
        self.last_prompt_for(DEFAULT_TRANSCRIPT_KEY).await
    }

    /// Log a `SessionEntry::MemoryWrite` for the user channel (AD-15).
    ///
    /// The TUI's `/remember` command writes directly through a
    /// `MemoryManager` it constructs itself (the engine's store is
    /// behind an `Arc` and is not the same manager instance the CLI
    /// builds). Rather than route the write through the engine — a
    /// larger refactor — the TUI calls this method after a successful
    /// write so the JSONL audit trail is uniform across all three
    /// channels: extraction, tool, and user.
    ///
    /// No-op when no recorder is installed (the CLI default).
    pub async fn record_user_memory_write(&self, memory_id: &str, tags: Vec<String>) {
        if let Ok(guard) = self.session_recorder.read()
            && let Some(rec) = guard.as_ref()
        {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let entry = kod_core_state::session_log::SessionEntry::MemoryWrite {
                timestamp_ms: now_ms,
                memory_id: memory_id.to_string(),
                channel: "user".to_string(),
                tags,
            };
            let _ = rec.record(&entry);
        }
    }

    /// This engine's session identity. Stable across the engine's
    /// lifetime; the value used to attribute auto-extracted episodic
    /// facts (design D2.5).
    pub fn session_id(&self) -> &kod_types::SessionId {
        &self.session_id
    }

    /// The session id that a transcript key belongs to.
    ///
    /// A swarm-agent transcript key is `"swarm:<uuid>"`; the UUID is
    /// the agent's, and an agent's auto-extracted facts are
    /// attributable to that agent. Any other key (including the
    /// interactive session's `""`) maps to the engine's own
    /// `session_id`.
    ///
    /// Public so a caller (a filter, a debug command) can ask the
    /// same question without knowing the key format.
    pub fn session_id_for_holder(&self, key: &str) -> kod_types::SessionId {
        if let Some(rest) = key.strip_prefix("swarm:")
            && let Ok(uuid) = uuid::Uuid::parse_str(rest)
        {
            return kod_types::SessionId::from_uuid(uuid);
        }
        self.session_id.clone()
    }

    /// The prompt the provider received on the most recent `process*`
    /// call for `key`.
    pub async fn last_prompt_for(&self, key: &str) -> Option<String> {
        self.last_prompt
            .read()
            .await
            .get(key)
            .map(|t| t.text.clone())
    }

    /// The full prompt trace (text + per-section allocation) for the
    /// most recent `process*` call on the default transcript.
    /// `PromptTrace::alloc` is what `/debug tokens` renders as the
    /// budget table; `PromptTrace::text` is byte-identical to what
    /// `last_prompt` returns.
    pub async fn last_prompt_trace(&self) -> Option<kod_core_state::budget::PromptTrace> {
        self.last_prompt_trace_for(DEFAULT_TRANSCRIPT_KEY).await
    }

    /// The full prompt trace for `key`.
    pub async fn last_prompt_trace_for(
        &self,
        key: &str,
    ) -> Option<kod_core_state::budget::PromptTrace> {
        self.last_prompt.read().await.get(key).cloned()
    }

    /// Point a transcript at a different working directory (D4-D1).
    /// Called by the swarm runner before an agent runs, with the
    /// agent's worktree path. Idempotent. Passing `None` clears the
    /// override for that key.
    pub async fn set_transcript_working_dir(&self, key: &str, dir: Option<PathBuf>) {
        let mut guard = self.transcript_working_dirs.write().await;
        match dir {
            Some(p) => {
                guard.insert(key.to_string(), p);
            }
            None => {
                guard.remove(key);
            }
        }
    }

    /// The working directory a transcript's tool calls run against.
    /// The per-key override when present, otherwise the engine-wide
    /// root.
    pub async fn working_dir_for(&self, key: &str) -> PathBuf {
        self.transcript_working_dirs
            .read()
            .await
            .get(key)
            .cloned()
            .unwrap_or_else(|| self.working_dir.clone())
    }

    /// Delta section 11.11: relocate a transcript into a fresh
    /// worktree that carries the current working tree's uncommitted
    /// changes.
    ///
    /// Returns the new worktree's path and branch. `clean_source =
    /// true` gives a worktree from HEAD with no dirty state.
    ///
    /// The current transcript working dir (the override, or the
    /// engine-wide root) is the repository the worktree branches
    /// from. After creation the transcript's working-dir override is
    /// pointed at the new worktree, so every subsequent tool call
    /// runs there. The original repo is untouched.
    ///
    /// Not in the engine's tool context: `/wt` is a user action, not
    /// a model action, and the model should not be able to relocate
    /// a session on its own.
    pub async fn relocate_to_session_worktree(
        &self,
        key: &str,
        clean_source: bool,
    ) -> Result<kod_core_quality::worktree::WorktreeInfo> {
        let repo = self.working_dir_for(key).await;
        let mut mgr = kod_core_quality::worktree::WorktreeManager::detect(&repo)
            .map_err(|e| {
                KodError::Internal(format!(
                    "could not inspect {} for worktree support: {e}",
                    repo.display()
                ))
            })?
            .ok_or_else(|| {
                KodError::InvalidState(format!(
                    "{} is not a git repository; a session worktree needs one",
                    repo.display()
                ))
            })?;
        let info = mgr.create_session(clean_source)?;
        // The manager's `Drop` removes every worktree it created on
        // the way out; a session worktree must outlive the manager,
        // so hand ownership off before it drops. The on-disk
        // ownership marker is what a future reaper consults.
        mgr.disarm();
        self.set_transcript_working_dir(key, Some(info.path.clone()))
            .await;
        Ok(info)
    }

    /// Clear the per-transcript working directory override for `key`.
    /// Called by the swarm runner after an agent's worktree is
    /// removed.
    pub async fn clear_transcript_working_dir(&self, key: &str) {
        self.transcript_working_dirs.write().await.remove(key);
    }

    /// Register a per-transcript write set (D4.2). `Some(globs)`
    /// restricts the transcript's tool calls to paths matching at
    /// least one glob; `None` removes the restriction (the default
    /// for every transcript). Called by the swarm runner before
    /// each agent starts.
    pub async fn set_transcript_write_globs(&self, key: &str, globs: Option<Vec<String>>) {
        let mut guard = self.transcript_write_globs.write().await;
        match globs {
            Some(g) if !g.is_empty() => {
                guard.insert(key.to_string(), Some(g));
            }
            // An empty `Some(vec![])` and a `None` mean the same
            // thing: no claim in force. Normalising here means the
            // tool-context builder has one case to check, not two.
            _ => {
                guard.remove(key);
            }
        }
    }

    /// The write set that applies to a transcript's tool calls.
    /// `None` when no claim is in force.
    pub async fn write_globs_for(&self, key: &str) -> Option<Vec<String>> {
        self.transcript_write_globs
            .read()
            .await
            .get(key)
            .cloned()
            .flatten()
    }

    /// Clear the per-transcript write set for `key`. Called by the
    /// swarm runner after each agent finishes.
    /// Seed a turn into the default transcript. Used by the TUI after
    /// restoring a saved session.
    pub async fn seed_turn(&self, user: bool, text: &str) {
        self.seed_turn_for(DEFAULT_TRANSCRIPT_KEY, user, text).await
    }

    /// Seed a turn into the transcript identified by `key`.
    pub async fn seed_turn_for(&self, key: &str, user: bool, text: &str) {
        self.record_turn_for(key, user, text).await;
    }

    /// Pin or unpin the most-recent transcript turn whose content
    /// matches `content` exactly. Returns `true` if a match was
    /// found.
    ///
    /// Matches newest-to-oldest so a re-issued prompt pins the most
    /// recent occurrence — the one the user just clicked.
    ///
    /// Keyed by content rather than index because the TUI's message
    /// list and the engine's transcript are not the same length (the
    /// TUI renders tool rows and system notices the engine never
    /// saw). Content equality is the one identity both sides share.
    pub async fn set_turn_pinned_by_content(&self, key: &str, content: &str, pinned: bool) -> bool {
        let mut history = self.history.write().await;
        let Some(turns) = history.get_mut(key) else {
            return false;
        };
        for t in turns.iter_mut().rev() {
            if t.content == content {
                t.metadata.pinned = pinned;
                return true;
            }
        }
        false
    }

    /// Clear the default transcript and short-term memory (`/clear`).
    /// See the doc on [`KodEngine::clear_history_for`] for the
    /// reasoning; this wrapper exists so the `/clear` command keeps its
    /// current shape.
    pub async fn clear_history(&self) {
        self.clear_history_for(DEFAULT_TRANSCRIPT_KEY).await;
        self.router.clear_short_term_memory().await;
    }

    /// Forget the transcript for `key`. Does NOT touch short-term
    /// memory: a per-key clear is used by the swarm runner between
    /// runs, and clearing the shared working set would discard turns a
    /// concurrent single-agent session still wants.
    pub async fn clear_history_for(&self, key: &str) {
        self.history.write().await.remove(key);
        self.last_prompt.write().await.remove(key);
        // Delta §11.4: a clear is a session boundary for async
        // delivery. Drop every background-job result queued for this
        // owner — a job that finished before the clear must not
        // surface in the post-reset transcript, which is the whole
        // point of the epoch guard (`AsyncDelivery::bump_epoch`'s
        // doc). The production code never bumped the epoch, so the
        // guard was dead; per-owner discard is the fix, and it leaves
        // a sibling swarm agent's queue intact.
        self.async_delivery.lock().discard_owner(key);
        // Delta §12.7: a cleared transcript is a new session for the
        // mental models. Cross the transcript boundary so
        // `AfterConsolidation`-triggered models reload at the next
        // fill; the previous blocks are kept until re-filled (a
        // stale-but-stable summary beats an empty one).
        self.mental_models.write().await.begin_transcript();
    }

    /// Drop both the transcript and its stored last-prompt for `key`.
    /// Used by the swarm runner at the end of a run so transcripts do
    /// not accumulate.
    /// H-T8: drop the last `count` turns from the transcript for
    /// `key`. Used by the TUI's `/regenerate` and `/delete`, which
    /// pre-fix rewound only the *display* (`KodApp::drop_last_exchange`)
    /// and left the engine transcript untouched — the next prompt
    /// went to the model with the "deleted" exchange still present,
    /// and `/regenerate` generated on top of the old answer.
    ///
    /// `count` is clamped to the current length. Dropping from an
    /// empty transcript is a no-op.
    pub async fn forget_last_turns_for(&self, key: &str, count: usize) {
        if count == 0 {
            return;
        }
        let mut hist = self.history.write().await;
        if let Some(turns) = hist.get_mut(key) {
            let drop = count.min(turns.len());
            turns.truncate(turns.len() - drop);
        }
    }

    /// Drop every piece of state the engine holds for `key`.
    ///
    /// Called by the swarm runner before a retry reuses a transcript
    /// key, and by the end-of-run cleanup for every dispatch. The
    /// engine holds a *lot* of per-transcript maps — history,
    /// gauges, tool-loop guards, todo trackers, plans, prewalks,
    /// write globs, working dir overrides, blackboard-viewer
    /// subscriptions, native-compaction blocks, image frames, and
    /// more. If any one of them is not cleared here, a long-lived
    /// daemon leaks one entry per dispatch per run.
    ///
    /// The one field this does *not* clear is `cancels`. Its
    /// entries carry a fire epoch the retry path uses to distinguish
    /// "a cancel that predates this attempt" from "a cancel that
    /// arrived during it"; clearing it here would erase a
    /// coordinator stop that landed mid-attempt and let a retry run
    /// on an agent the user already stopped. Callers that need to
    /// forget a cancel call [`Self::clear_cancel_for`] explicitly.
    pub async fn forget_transcript(&self, key: &str) {
        // Transcript bodies and their rendered form.
        self.history.write().await.remove(key);
        self.last_prompt.write().await.remove(key);

        // Context accounting: the gauge anchored on a transcript that
        // no longer exists.
        self.context_gauges.write().await.remove(key);
        self.observed_usage.write().await.remove(key);

        // Compaction: the summary that was going to be written, the
        // in-flight marker, the provider-native block, the frame
        // list.
        self.pending_summaries.write().await.remove(key);
        self.summaries_in_flight.write().await.remove(key);
        self.native_compaction_blocks.write().await.remove(key);
        self.image_frames.write().await.remove(key);

        // Memory: the retention cursor and the injection-TTL map.
        self.retention_cursors.write().await.remove(key);
        self.injected_memory_at.write().await.remove(key);
        self.decisions_cursors.write().await.remove(key);

        // Prompt-building: prewarmed flag, tool surface fingerprint,
        // Jev filter state, fidelity cache, current request text.
        self.prewarmed.write().await.remove(key);
        self.tool_surface_fingerprint.write().await.remove(key);
        self.tool_filter_states.write().await.remove(key);
        self.fidelity_cache.write().await.remove(key);
        self.current_requests.write().await.remove(key);

        // Round-loop guards and nudges.
        self.tool_loop_guards.write().await.remove(key);
        self.todo_trackers.write().await.remove(key);

        // Plan mode, plans, and their reference paths.
        self.plans.write().await.remove(key);
        self.plan_reference_paths.write().await.remove(key);
        self.plan_mode.write().await.remove(key);
        self.prewalks.write().await.remove(key);

        // Durable decisions for the transcript.
        self.decision_logs.write().await.remove(key);

        // Swarm dispatch state: per-transcript working-dir override,
        // write-set claim, blackboard subscription.
        self.transcript_working_dirs.write().await.remove(key);
        self.transcript_write_globs.write().await.remove(key);
        self.pending_trace_parent.write().await.remove(key);
        self.blackboard_viewers.write().await.remove(key);

        // Pending soft interrupts queued for this transcript. A
        // dispatch that failed mid-turn may have left a swarm
        // conflict notice or a followup in the queue; the next
        // attempt must not see it.
        self.steers.write().await.remove(key);
    }

    /// Read-only snapshot of the stored transcript for `key`.
    ///
    /// Returns turns verbatim, including tool rows and empty
    /// assistant turns that `render_history_for` filters out. This
    /// is the raw transcript as the engine stores it, useful for
    /// tests that inspect the effects of a mutation and for any
    /// future diagnostic surface that wants to see what actually
    /// exists rather than the prompt-shaped view.
    pub async fn history_for(&self, key: &str) -> Vec<kod_types::ChatMessage> {
        self.history
            .read()
            .await
            .get(key)
            .cloned()
            .unwrap_or_default()
    }

    /// Compact the default transcript to the last `max_turns` turns.
    pub async fn compact_history(&self, max_turns: usize) {
        self.compact_history_for(DEFAULT_TRANSCRIPT_KEY, max_turns)
            .await
    }

    /// Compact the transcript for `key` to the last `max_turns` turns.
    ///
    /// The dropped prefix is **summarised**, not discarded. An explicit
    /// `/compact` used to `drain` the front of the transcript and leave
    /// nothing behind, so every fact, file path and decision from those
    /// turns vanished from the model's view — while the automatic
    /// threshold path summarised the same kind of prefix. Two paths
    /// doing the same job with different consequences is the bug; this
    /// makes the manual path match the automatic one.
    ///
    /// The summary is the same factual `emergency_summary` the
    /// hard-threshold path falls back to: counts, tool names, and file
    /// paths, with no attempt to explain intent. It is inserted as a
    /// pinned message so the render path cannot drop it for budget
    /// reasons — losing it would lose the only record of what was
    /// compacted away.
    ///
    /// A compaction removes messages from the front of the transcript,
    /// which is exactly the prefix the context gauge anchored on. The
    /// gauge is cleared here so the next settled call re-anchors on
    /// the new transcript; without the clear, the gauge's `estimate`
    /// would keep adding a tail onto a prefix the provider is no
    /// longer charging for.
    pub async fn compact_history_for(&self, key: &str, max_turns: usize) {
        // The window sizes the summary's header text. `0` when no
        // registry is installed; `emergency_summary` renders that as
        // "context exceeded 0 tokens", which would be misleading, so
        // the budget is clamped to something non-zero.
        let window = self
            .budget_hint
            .read()
            .map(|g| g.0 as u64)
            .unwrap_or(0)
            .max(1);

        // Take and release the history lock before touching the gauge,
        // so a lock-ordering bug between the two maps is impossible.
        let shrank = {
            let mut history = self.history.write().await;
            match history.get_mut(key) {
                Some(turns) if turns.len() > max_turns => {
                    let drop = turns.len() - max_turns;
                    let dropped: Vec<kod_types::ChatMessage> = turns[..drop].to_vec();
                    // Rotate rather than drain: the retained suffix
                    // keeps its allocation, and the dropped prefix is
                    // still owned long enough to summarise.
                    turns.rotate_left(drop);
                    turns.truncate(turns.len() - drop);

                    let summary = crate::compaction::emergency_summary(&dropped, window);
                    let mut summary_msg = kod_types::ChatMessage::text(
                        kod_types::MessageId::new(),
                        kod_types::MessageRole::User,
                        format!("## Previous Conversation Summary\n{summary}"),
                        time::OffsetDateTime::now_utc(),
                    );
                    // Pinned so the render path never drops it for
                    // budget reasons.
                    summary_msg.metadata.pinned = true;
                    turns.insert(0, summary_msg);
                    true
                }
                _ => false,
            }
        };
        if shrank {
            self.context_gauges.write().await.remove(key);
        }
    }

    /// Shared classification + prompt-build pipeline used by every
    /// `process_*` entry point.
    ///
    /// `retrieval_log_turn_id` is threaded through to
    /// [`KodEngine::classify_and_filter`]; `None` skips the retrieval
    /// log write (the goal path's choice), `Some(0)` is the collected
    /// path's placeholder, `Some(real_id)` is the streaming path.
    ///
    /// `create_plan` controls the Tier 2.1 model-authored plan. Only
    /// the collected path asks the model to plan on the first turn of
    /// a Complex/MultiStep task; the streaming paths rely on the model
    /// to reach for tools itself. See `process_for` for rationale.
    ///
    /// Ordering matters: the plan (if created) must be written to
    /// `self.plans` *before* `build_prompt_plan` renders `system_text`,
    /// otherwise the plan never reaches the model.
    /// Write the per-turn `PromptTrace` that powers `/debug last-prompt`
    /// and `/debug tokens`.
    ///
    /// Kept separate from `prepare_turn` because the goal path augments
    /// the prompt with a `## Goal` block *after* the pipeline runs — the
    /// snapshot must reflect what the model actually sees on turn 1, not
    /// what it saw before the goal was prepended. Callers pass their own
    /// final `pending` text and the allocation the pipeline produced.
    pub(crate) async fn snapshot_prompt(
        &self,
        key: &str,
        pending: &str,
        alloc: &std::result::Result<
            kod_core_state::budget::Allocation,
            kod_core_state::budget::BudgetError,
        >,
    ) {
        let trace = kod_core_state::budget::PromptTrace {
            text: pending.to_string(),
            alloc: alloc.as_ref().ok().copied(),
        };
        self.last_prompt
            .write()
            .await
            .insert(key.to_string(), trace);
    }

    /// Build the hook `execute_command` calls when the model sets
    /// `run_in_background` (P2-d).
    ///
    /// The hook spawns the command with its output going to a spool
    /// file, registers a job, and launches a watcher that fires a
    /// `SoftInterrupt::background` on completion or — when the caller
    /// set `stall_wake_seconds` — on a silence long enough to mean the
    /// command is wedged. The interrupt rides the same steer channel a
    /// user note does, so it lands at the next round boundary without
    /// the watcher needing the engine.
    ///
    /// Returns `None` when the spool cannot be created or the spawn
    /// fails, which the tool reads as "run it inline."

    /// Fire a speculative request that warms the provider's connection
    /// and its KV-cache prefix.
    ///
    /// Called when the user starts typing a fresh turn, so the real
    /// request — seconds later, once they finish composing — reads a
    /// cache that is already written. For a local model the same
    /// request keeps the weights resident, which is the larger win:
    /// `ollama` unloads after five idle minutes and the first real
    /// turn otherwise pays a full reload.
    ///
    /// **The economics are not free.** A request that writes the cache
    /// pays ~1.25x on the prefix; the real turn then reads at ~0.1x.
    /// That is a loss if the turn follows immediately and a win once
    /// the user has typed for a few seconds — which is why the caller
    /// fires on the first keystroke of a fresh turn, not on submit.
    ///
    /// One-shot per transcript: the latch is set here and cleared when
    /// a real turn begins. A run of keystrokes warms once.
    ///
    /// Never awaited by the caller. The request is discarded: this
    /// exists for its side effects on the provider, not for its
    /// output.

    /// A read-only snapshot of a transcript, for a client that wants
    /// to see what a session is doing **without attaching to it**.
    ///
    /// Attaching would disturb the very session being previewed — the
    /// peek must not touch the history, the steers, or the cancel
    /// flag. This reads the history under a shared lock, renders the
    /// last few turns, and returns. Nothing is mutated.
    ///
    /// `max_chars` bounds the tail: a peek is for orientation, and a
    /// caller that wants the whole transcript can read the session
    /// log.
    pub async fn peek_transcript(&self, key: &str, max_chars: usize) -> serde_json::Value {
        let (count, tail) = {
            let guard = self.history.read().await;
            let turns = guard.get(key);
            let count = turns.map(|t| t.len()).unwrap_or(0);
            let mut out = String::new();
            if let Some(turns) = turns {
                // The last 20 turns, newest-last, matching the order
                // the model would see them.
                for m in turns.iter().rev().take(20).rev() {
                    // Tool rows are structural; a peek shows prose.
                    if matches!(m.role, kod_types::MessageRole::Tool) {
                        continue;
                    }
                    out.push_str(&m.render_text());
                    out.push('\n');
                }
            }
            (count, out)
        };

        let total = tail.chars().count();
        let tail = if total > max_chars {
            tail.chars().skip(total - max_chars).collect::<String>()
        } else {
            tail
        };

        serde_json::json!({
            "key": key,
            "messages": count,
            "tail": tail,
            "truncated": total > max_chars,
        })
    }
}

impl KodEngine {
    /// Test-only: total number of entries across every
    /// transcript-scoped map on the engine. A non-zero count *for a
    /// key that should have been forgotten* is the signal that a
    /// leak exists; tests assert the total is zero after a run.
    ///
    /// Reads every map `forget_transcript` writes to (plus
    /// `blackboard_viewers`, which is a set, not a map). Kept in sync
    /// with `forget_transcript` by construction — the pair is the
    /// point of the invariant.
    pub async fn transcript_scoped_key_count_for_tests(&self) -> usize {
        let mut n = 0usize;
        n += self.history.read().await.len();
        n += self.last_prompt.read().await.len();
        n += self.context_gauges.read().await.len();
        n += self.observed_usage.read().await.len();
        n += self.pending_summaries.read().await.len();
        n += self.summaries_in_flight.read().await.len();
        n += self.native_compaction_blocks.read().await.len();
        n += self.image_frames.read().await.len();
        n += self.retention_cursors.read().await.len();
        n += self.injected_memory_at.read().await.len();
        n += self.decisions_cursors.read().await.len();
        n += self.prewarmed.read().await.len();
        n += self.tool_surface_fingerprint.read().await.len();
        n += self.tool_filter_states.read().await.len();
        n += self.fidelity_cache.read().await.len();
        n += self.current_requests.read().await.len();
        n += self.tool_loop_guards.read().await.len();
        n += self.todo_trackers.read().await.len();
        n += self.plans.read().await.len();
        n += self.plan_reference_paths.read().await.len();
        n += self.plan_mode.read().await.len();
        n += self.prewalks.read().await.len();
        n += self.decision_logs.read().await.len();
        n += self.transcript_working_dirs.read().await.len();
        n += self.transcript_write_globs.read().await.len();
        n += self.pending_trace_parent.read().await.len();
        n += self.blackboard_viewers.read().await.len();
        n += self.steers.read().await.len();
        n
    }
}

impl KodEngine {
    /// Test-only: how many async-job results are queued for `key`.
    pub async fn queued_async_results_for_tests(&self, key: &str) -> usize {
        self.async_delivery.lock().queued_for(key)
    }

    /// Test-only: enqueue a job result as if a background job had
    /// just finished. Used to prove `clear_history_for` drops them.
    pub async fn enqueue_async_result_for_tests(&self, owner_id: &str, job_id: u64, body: &str) {
        let epoch = self.async_delivery.lock().epoch();
        self.async_delivery
            .lock()
            .enqueue(crate::async_delivery::AsyncResult {
                job_id,
                owner_id: owner_id.to_string(),
                kind: "test".to_string(),
                body: body.to_string(),
                artifact: None,
                epoch,
            });
    }

    /// Test-only: the single transcript key with a write-glob claim,
    /// and the globs, when exactly one entry exists. `None` otherwise
    /// (zero or more than one). Used by tests that drive one agent
    /// and want to observe the engine's write-set state without
    /// knowing the dispatch key.
    pub async fn only_write_globs_for_tests(&self) -> Option<(String, Vec<String>)> {
        let guard = self.transcript_write_globs.read().await;
        if guard.len() != 1 {
            return None;
        }
        let (k, v) = guard.iter().next()?;
        let globs = v.clone()?;
        Some((k.clone(), globs))
    }

    /// Test-only: every transcript key currently holding a request
    /// that looks like a swarm dispatch (`swarm:*`). Lets a test
    /// provider find the key of the dispatch it is running under, so
    /// it can cancel it mid-attempt.
    pub async fn swarm_dispatch_keys_for_tests(&self) -> Vec<String> {
        self.current_requests
            .read()
            .await
            .keys()
            .filter(|k| k.starts_with("swarm:"))
            .cloned()
            .collect()
    }

    /// Test-only: the single transcript key with a working-dir
    /// override, when exactly one entry exists. `None` otherwise.
    pub async fn only_working_dir_for_tests(&self) -> Option<(String, PathBuf)> {
        let guard = self.transcript_working_dirs.read().await;
        if guard.len() != 1 {
            return None;
        }
        let (k, v) = guard.iter().next()?;
        Some((k.clone(), v.clone()))
    }
}
