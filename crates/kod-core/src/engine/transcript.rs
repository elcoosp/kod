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
        let short = if text.len() > MAX_TURN_CHARS {
            format!("{}… [truncated]", truncate_chars(text, MAX_TURN_CHARS))
        } else {
            text.to_string()
        };
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
    pub async fn clear_transcript_write_globs(&self, key: &str) {
        self.transcript_write_globs.write().await.remove(key);
    }

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

    pub async fn forget_transcript(&self, key: &str) {
        self.history.write().await.remove(key);
        self.last_prompt.write().await.remove(key);
        // The transcript this gauge anchored on is gone; drop the
        // anchor with it.
        self.context_gauges.write().await.remove(key);
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
    /// A compaction removes messages from the front of the transcript,
    /// which is exactly the prefix the context gauge anchored on. The
    /// gauge is cleared here so the next settled call re-anchors on
    /// the new transcript; without the clear, the gauge's `estimate`
    /// would keep adding a tail onto a prefix the provider is no
    /// longer charging for.
    pub async fn compact_history_for(&self, key: &str, max_turns: usize) {
        // Take and release the history lock before touching the gauge,
        // so a lock-ordering bug between the two maps is impossible.
        let shrank = {
            let mut history = self.history.write().await;
            if let Some(turns) = history.get_mut(key)
                && turns.len() > max_turns
            {
                let drop = turns.len() - max_turns;
                turns.drain(..drop);
                true
            } else {
                false
            }
        };
        if shrank {
            self.context_gauges.write().await.remove(key);
        }
    }
}
