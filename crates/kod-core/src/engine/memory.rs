use super::*;

impl KodEngine {
    /// Delta §12.6: continuous memory extraction.
    ///
    /// Runs at a turn boundary. The per-transcript retention cursor
    /// answers "what is new since the last pass"; when the new tail
    /// passes the cadence floor, the tail is extracted and stored as
    /// episodic entries. On a cursor reset (a rewind, a branch, an
    /// in-place edit) nothing is extracted this turn — the cursor now
    /// covers the whole transcript, and the next pass is incremental
    /// from there.
    ///
    /// Best-effort: every failure logs and returns. A turn must never
    /// fail because memory extraction did.
    ///
    /// # Latency
    ///
    /// The extraction is one small model call, run inline. A caller
    /// that wants it off the turn's critical path spawns it instead;
    /// backgrounding it needs the engine state cloned into a task and
    /// is a documented follow-up.
    pub(crate) async fn maybe_extract_continuously(&self, key: &str) {
        if !self.router.has_memory() {
            return;
        }
        let config = match kod_config::KodConfig::load_cached() {
            Ok(c) => c,
            Err(_) => return,
        };
        // WS-B: extraction is a policy choice. `off` silences every
        // pass here; `shutdown` keeps only the shutdown pass.
        if !matches!(
            config.memory.extraction_mode,
            kod_config::memory::ExtractionMode::Continuous
        ) {
            return;
        }
        let max_entries = config.memory.extract_max_entries.max(1);
        let cadence = kod_memory::retention::RetentionCadence {
            every_n_turns: config.memory.extraction_every_n_turns.max(1),
            min_new_messages: config.memory.extraction_min_messages,
        };

        let transcript: Vec<kod_types::ChatMessage> = {
            let history = self.history.read().await;
            history.get(key).cloned().unwrap_or_default()
        };
        if transcript.is_empty() {
            return;
        }

        // The cursor decides what is new. `advance` mutates the cursor,
        // so read the old length first to know the new tail's range.
        let (new_start, new_len, new_count) = {
            let mut cursors = self.retention_cursors.write().await;
            let cursor = cursors.entry(key.to_string()).or_default();
            let old = cursor.retained();
            match cursor.advance(&transcript) {
                Some(count) => (old, cursor.retained(), count),
                None => return,
            }
        };
        // The cadence floor: both the user-turn count and the
        // message count must pass. A tool-using turn appends several
        // messages per single user turn, so the message floor alone
        // fired on effectively every turn (the old dead-code bug).
        let new_user_turns = transcript[new_start..new_len]
            .iter()
            .filter(|m| matches!(m.role, kod_types::MessageRole::User))
            .count();
        if !cadence.is_due(new_count, new_user_turns) {
            return;
        }

        let chain = self.resolve_chain_for_task("Simple").await;
        let Some(model_ref) = chain.first() else {
            return;
        };
        let provider = match self.resolve_provider_for_model_ref(model_ref).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "continuous extraction: no provider");
                return;
            }
        };
        let tail = &transcript[new_start..new_len];
        let facts = match kod_memory::extract::extract(provider, model_ref, tail, max_entries).await
        {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(error = %e, "continuous extraction: failed");
                return;
            }
        };
        if facts.is_empty() {
            return;
        }
        let project_key = Some(crate::router::TaskRouter::project_key_for(
            &self.working_dir,
        ));
        let mut stored = 0usize;
        for fact in &facts {
            let mut metadata = kod_memory::extract::metadata_for(fact, project_key.clone());
            metadata.session_id = Some(self.session_id_for_holder(key));
            match self.router.store_episodic(&fact.content, metadata).await {
                Ok(_) => stored += 1,
                Err(e) => {
                    tracing::warn!(error = %e, "continuous extraction: store failed");
                }
            }
        }
        // WS-B visibility: one info line per extraction so operators
        // can see fact collection working (`kod memory list` shows the
        // stored entries).
        tracing::info!(
            key,
            new_messages = new_count,
            new_user_turns,
            stored,
            total = facts.len(),
            "continuous extraction pass",
        );
    }

    /// Delta §12.3: friction-gated decision extraction.
    ///
    /// Runs at a turn boundary, per user prompt. Extracts decisions
    /// from the *user's* message — the design's "sharpshooter"
    /// position — and admits them through the grounding gate: an
    /// extracted `evidence` string must appear verbatim (case
    /// insensitive) in the prompt it came from. A model that invented
    /// the evidence has invented the decision; the gate drops it
    /// before it reaches the queue.
    ///
    /// The admitted deltas go into `sharpshooter_deltas`. A follow-up
    /// consolidation pass ranks them by friction and rewrites
    /// `architecture.md` / `product.md` / `style.md` under the
    /// design's 120-line ceiling per file.
    ///
    /// Best-effort: every failure logs and returns. A turn must never
    /// fail because decision extraction did.
    pub(crate) async fn maybe_extract_decisions(&self, key: &str) {
        if !self.router.has_memory() {
            return;
        }
        // Find the user prompt for this turn: the last User-role
        // message in the transcript. The engine records the message
        // at the start of the turn; a swarm agent's assistant text
        // never appears here, so this picks the human's words.
        let user_prompt: String = {
            let history = self.history.read().await;
            let Some(h) = history.get(key) else {
                return;
            };
            let mut found = String::new();
            for m in h.iter().rev() {
                if matches!(m.role, kod_types::MessageRole::User) {
                    found = m.content.clone();
                    break;
                }
            }
            found
        };
        if !kod_memory::sharpshooter::prompt_is_eligible(&user_prompt) {
            return;
        }
        // WS-B: the decision extractor is gated and cadenced per
        // transcript, mirroring the retention cursor. Ineligible
        // prompts never touch the counter.
        let decisions_every_n_turns = match kod_config::KodConfig::load_cached() {
            Ok(c) => {
                if !c.memory.decisions_enabled {
                    return;
                }
                c.memory.decisions_every_n_turns.max(1)
            }
            Err(_) => 1,
        };
        {
            let mut cursors = self.decisions_cursors.write().await;
            let seen = cursors.entry(key.to_string()).or_insert(0);
            *seen += 1;
            if *seen < decisions_every_n_turns {
                return;
            }
            *seen = 0;
        }

        let max_decisions = kod_memory::sharpshooter::DEFAULT_MAX_DECISIONS;

        let chain = self.resolve_chain_for_task("Simple").await;
        let Some(model_ref) = chain.first() else {
            return;
        };
        let provider = match self.resolve_provider_for_model_ref(model_ref).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "sharpshooter: no provider");
                return;
            }
        };

        let prompt = kod_memory::sharpshooter::build_prompt(&user_prompt, max_decisions);
        let opts = kod_provider::GenerationOptions {
            temperature: Some(0.1),
            max_tokens: Some(1024),
            ..Default::default()
        };
        let reply = match provider.generate(&prompt, &opts).await {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!(error = %e, "sharpshooter: extraction call failed");
                return;
            }
        };
        let parsed = kod_memory::sharpshooter::parse_reply(&reply, max_decisions);
        if parsed.is_empty() {
            return;
        }
        // The admission gate runs *after* the parse: parse drops
        // shape-malformed deltas, admit drops ungrounded ones.
        let admitted: Vec<kod_memory::sharpshooter::DecisionDelta> = parsed
            .into_iter()
            .filter(|d| kod_memory::sharpshooter::admit(d, &user_prompt).is_some())
            .collect();
        if admitted.is_empty() {
            return;
        }
        let n = admitted.len();
        self.sharpshooter_deltas.write().await.extend(admitted);
        tracing::info!(key, admitted = n, "sharpshooter: decision deltas admitted");
    }

    /// Delta §12.3: a snapshot of the admitted decision deltas. Used
    /// by a follow-up consolidation pass, and by tests. Cloning is
    /// cheap: the queue holds at most a handful of deltas per
    /// session.
    pub async fn sharpshooter_deltas(&self) -> Vec<kod_memory::sharpshooter::DecisionDelta> {
        self.sharpshooter_deltas.read().await.clone()
    }

    /// Delta §12.3: take the queue, leaving it empty. Used by the
    /// consolidation pass; a caller that wants a copy uses
    /// [`Self::sharpshooter_deltas`].
    pub(crate) async fn sharpshooter_drain(&self) -> Vec<kod_memory::sharpshooter::DecisionDelta> {
        std::mem::take(&mut *self.sharpshooter_deltas.write().await)
    }

    /// Delta §12.3: the consolidation pass. Drains the delta queue,
    /// groups by the target file each kind names, and asks the small
    /// model to rewrite each file. Files live under
    /// `<working_dir>/.kod/decisions/<file>` so a repo carries its
    /// own conventions. Enforces [`kod_memory::sharpshooter::FILE_LINE_CEILING`]
    /// after the model call, so a reply that ignores the prompt's
    /// limit is still bounded.
    ///
    /// Best-effort: a model error on one file logs and continues to
    /// the next; a write failure does the same. Returns the number
    /// of files that were rewritten.
    pub async fn consolidate_sharpshooter_now(&self) -> usize {
        let drained = self.sharpshooter_drain().await;
        if drained.is_empty() {
            return 0;
        }
        // Group by the target file each delta kind names. Ranking is
        // per-group: the model sees its group's deltas highest
        // friction first.
        let ranked = kod_memory::sharpshooter::rank_by_friction(drained);
        let mut groups: std::collections::HashMap<&'static str, Vec<_>> =
            std::collections::HashMap::new();
        for d in ranked {
            groups.entry(d.kind.target_file()).or_default().push(d);
        }

        let chain = self.resolve_chain_for_task("Simple").await;
        let Some(model_ref) = chain.first() else {
            return 0;
        };
        let provider = match self.resolve_provider_for_model_ref(model_ref).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "sharpshooter consolidation: no provider");
                return 0;
            }
        };

        let dir = self.working_dir.join(".kod/decisions");
        if let Err(e) = std::fs::create_dir_all(&dir) {
            tracing::warn!(error = %e, "sharpshooter consolidation: mkdir failed");
            return 0;
        }

        let opts = kod_provider::GenerationOptions {
            temperature: Some(0.1),
            max_tokens: Some(2048),
            ..Default::default()
        };
        let mut written = 0usize;
        for (file, deltas) in groups {
            let path = dir.join(file);
            let existing = std::fs::read_to_string(&path).unwrap_or_default();
            let prompt = kod_memory::sharpshooter::build_consolidation_prompt(&existing, &deltas);
            let reply = match provider.generate(&prompt, &opts).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(error = %e, file, "sharpshooter: consolidation call failed");
                    continue;
                }
            };
            let text = kod_memory::sharpshooter::truncate_to_ceiling(&reply);
            if text.trim().is_empty() {
                continue;
            }
            if let Err(e) = std::fs::write(&path, &text) {
                tracing::warn!(error = %e, file, "sharpshooter: write failed");
                continue;
            }
            tracing::debug!(
                file,
                bytes = text.len(),
                "sharpshooter: rewrote decisions file"
            );
            written += 1;
        }
        written
    }

    /// Run the memory extraction pass over the session transcript
    /// (D2-B3b). Best-effort: errors leave the store unchanged and
    /// return `Ok(0)`.
    ///
    /// Called by `shutdown()` when `memory.extract_on_shutdown` is
    /// true. Also callable directly from a test or a future
    /// `/remember-session` command.
    pub async fn extract_memories_now(&self, key: &str) -> Result<usize> {
        let config = match kod_config::KodConfig::load_cached() {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "extract_memories_now: config load failed");
                return Ok(0);
            }
        };
        let max_entries = config.memory.extract_max_entries.max(1);

        let transcript: Vec<kod_types::ChatMessage> = {
            let history = self.history.read().await;
            history.get(key).cloned().unwrap_or_default()
        };
        if transcript.is_empty() {
            return Ok(0);
        }

        let chain = self.resolve_chain_for_task("Simple").await;
        let Some(model_ref) = chain.first() else {
            return Ok(0);
        };
        let provider = match self.resolve_provider_for_model_ref(model_ref).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "extract_memories_now: no provider");
                return Ok(0);
            }
        };

        let facts =
            match kod_memory::extract::extract(provider, model_ref, &transcript, max_entries).await
            {
                Ok(f) => f,
                Err(e) => {
                    tracing::warn!(error = %e, "extract_memories_now: extraction failed");
                    return Ok(0);
                }
            };
        if facts.is_empty() {
            return Ok(0);
        }

        let project_key = Some(crate::router::TaskRouter::project_key_for(
            &self.working_dir,
        ));

        let mut stored = 0usize;
        for fact in &facts {
            let mut metadata = kod_memory::extract::metadata_for(fact, project_key.clone());
            // The design (D2.5) attributes every auto-extracted fact to
            // a session so consolidation can treat "an episode with no
            // touch in 60 days" as archivable without ever archiving a
            // durable `LongTerm` entry the user asked to remember. The
            // transcript key is already a string; a `SessionId` is a
            // UUID newtype, so a hash-shaped key degrades to `None` —
            // the fact is still stored, it just loses the attribution
            // a per-session triage would need.
            metadata.session_id = Some(self.session_id_for_holder(key));
            // Store as Episodic (not LongTerm): the extraction channel
            // is the auto path; only `memory_save` and the user's
            // `/remember` write the durable layer.
            match self
                .router
                .store_episodic(&fact.content, metadata.clone())
                .await
            {
                Ok(id) => {
                    // One `MemoryWrite` per stored fact (AD-15). The
                    // extraction channel is the auto path; `memory_save`
                    // and `/remember` log the "tool" and "user" channels
                    // respectively.
                    if let Ok(guard) = self.session_recorder.read()
                        && let Some(rec) = guard.as_ref()
                    {
                        let now_ms = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis() as u64)
                            .unwrap_or(0);
                        let entry = kod_core_state::session_log::SessionEntry::MemoryWrite {
                            timestamp_ms: now_ms,
                            memory_id: id.as_uuid().to_string(),
                            channel: "extraction".to_string(),
                            tags: metadata.tags.clone(),
                        };
                        let _ = rec.record(&entry);
                    }
                    stored += 1;
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        content = %fact.content,
                        "extract_memories_now: store failed; skipping fact"
                    );
                }
            }
        }
        tracing::info!(stored, total = facts.len(), "memory extraction complete");
        Ok(stored)
    }
}
