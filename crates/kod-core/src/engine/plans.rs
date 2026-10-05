use super::*;

impl KodEngine {
    /// On the first turn of a Complex or MultiStep task, ask the
    /// model for a step-by-step plan and store it (Tier 2.1).
    ///
    /// The plan prompt is deliberately short — the model already has
    /// the user's request as the input. The result is JSON-parsed
    /// with a tolerant parser (first `[` to last `]`), the same
    /// pattern `parse_subtasks` uses for swarm.
    ///
    /// Fail-silent: no plan is a valid outcome. A bad Jev signal, a
    /// short request, or a malformed reply all leave the session
    /// without a plan, which is the pre-2.1 behaviour.
    pub(crate) async fn maybe_create_plan(
        &self,
        key: &str,
        input: &str,
        task_type: crate::router::TaskType,
        provider: &Arc<dyn LlmProvider>,
        options: &GenerationOptions,
    ) {
        use crate::router::TaskType;
        if !matches!(task_type, TaskType::Complex | TaskType::MultiStep) {
            return;
        }
        // Do not overwrite an existing plan.
        if self.plans.read().await.contains_key(key) {
            return;
        }
        // A very short request cannot carry a plan worth generating.
        if input.len() < 30 {
            return;
        }
        let prompt = format!(
            "Produce a concise, ordered plan for the request below.\n\
             Output ONLY a JSON array of strings, one per step.\n\
             Rules:\n\
             - 3 to 8 steps.\n\
             - Each step is one imperative sentence.\n\
             - No explanations, no headers, no wrapping prose.\n\n\
             Request: {input}"
        );
        let raw = match provider.generate(&prompt, options).await {
            Ok(t) => t,
            Err(e) => {
                tracing::debug!(error = %e, "plan generation skipped");
                return;
            }
        };
        let Some(steps) = Self::parse_plan_steps(&raw) else {
            tracing::debug!("plan reply did not contain a JSON array");
            return;
        };
        if steps.is_empty() {
            return;
        }
        let plan = kod_core_state::plan::Plan::new(input, steps);
        let step_count = plan.steps.len();
        // Delta §11.10: autosave before moving the plan into the
        // map. The working-dir override for a swarm transcript is
        // honoured so a subagent's plan lands under its own worktree
        // — that is the directory a merge reads it from.
        self.autosave_plan_for(key).await;
        self.set_plan(key, plan).await;
        tracing::info!(steps = step_count, "plan created");
    }

    /// Delta §11.10: autosave the transcript's plan to
    /// `~/.kod/plans/<fnv1a-of-canonical-cwd>/`. Returns the path
    /// written, or `None` when there is no plan or the write failed.
    ///
    /// Best-effort by design: an autosave that failed must not fail
    /// the plan operation. The caller in `maybe_create_plan` ignores
    /// the return; a `/plan save` command can surface it.
    pub async fn autosave_plan_for(&self, key: &str) -> Option<std::path::PathBuf> {
        let plan = self.plans.read().await.get(key).cloned()?;
        let working_dir = self
            .transcript_working_dirs
            .read()
            .await
            .get(key)
            .cloned()
            .unwrap_or_else(|| self.working_dir.clone());
        let path = kod_core_state::plan::autosave_plan(&plan, &working_dir);
        if let Some(p) = &path {
            tracing::debug!(path = %p.display(), "plan autosaved");
        }
        path
    }

    /// Log one memory retrieval event (Tier 2.4). Called once per
    /// prompt that retrieves anything; a no-op when no recorder is
    /// installed or the retrieval was empty.
    pub(crate) fn log_memory_retrieval(
        &self,
        turn_id: u64,
        query: &str,
        retrieved: &[(String, f32)],
        considered: usize,
        dropped: &[(String, String)],
    ) {
        if retrieved.is_empty() {
            return;
        }
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        // Hash the query so the log carries a stable identity without
        // storing the (potentially sensitive) text itself.
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in query.as_bytes() {
            h ^= *b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        let entry = kod_core_state::session_log::SessionEntry::MemoryRetrieval {
            timestamp_ms: now_ms,
            turn_id,
            query_hash: format!("{h:016x}"),
            retrieved: retrieved.to_vec(),
            referenced: Vec::new(),
            user_corrected: false,
            considered,
            dropped: dropped.to_vec(),
        };
        if let Ok(guard) = self.session_recorder.read()
            && let Some(rec) = guard.as_ref()
            && let Err(e) = rec.record(&entry)
        {
            tracing::warn!(error = %e, "could not append MemoryRetrieval to session log");
        }
    }

    /// Tier 3.4 — classify a completed turn for durable decisions.
    ///
    /// Two Jev calls per turn, gated by the first: a yes/no question
    /// on the whole turn ("durable decision?"), then a score question
    /// on the kind. Cheap when the turn is a plain question, and
    /// produces one `DecisionRecord` when it is not.
    ///
    /// No-op when Jev is disabled, when either side of the exchange
    /// is trivially short, or when the classifier scores the turn
    /// below the threshold.
    /// The durable decisions for a transcript (Tier 3.4).
    /// The last `limit` decisions for `key`, newest-last, as plain
    /// text strings. Used by the swarm runner to seed a subagent's
    /// brief with the parent's durable state (P5).
    pub async fn recent_decisions(&self, key: &str, limit: usize) -> Vec<String> {
        let log = self.decisions_for(key).await;
        let start = log.entries.len().saturating_sub(limit);
        log.entries[start..]
            .iter()
            .map(|d| d.text.clone())
            .collect()
    }

    /// The rendered repomap text for the engine's working directory,
    /// if a map was built. Used by the swarm runner (P5) so a
    /// subagent's brief carries the same view of the repository the
    /// parent has.
    pub async fn repomap_text(&self) -> String {
        self.router
            .repo_map_text()
            .map(|s| s.as_str().to_string())
            .unwrap_or_default()
    }

    pub async fn decisions_for(&self, key: &str) -> kod_core_state::decisions::DecisionLog {
        self.decision_logs
            .read()
            .await
            .get(key)
            .cloned()
            .unwrap_or_default()
    }

    /// Replace the entire decision log for a transcript.
    pub async fn set_decision_log(&self, key: &str, log: kod_core_state::decisions::DecisionLog) {
        self.decision_logs
            .write()
            .await
            .insert(key.to_string(), log);
        self.persist_state().await;
    }

    /// Append a decision to a transcript's log.
    pub async fn add_decision(
        &self,
        key: &str,
        turn_id: u64,
        kind: kod_core_state::decisions::DecisionKind,
        text: String,
        author: kod_core_state::decisions::DecisionAuthor,
    ) -> u64 {
        let mut g = self.decision_logs.write().await;
        let log = g.entry(key.to_string()).or_default();
        let id = log.push(turn_id, kind, text, author);
        drop(g);
        self.persist_state().await;
        id
    }

    /// Drop a decision by id.
    pub async fn drop_decision(&self, key: &str, id: u64) -> bool {
        let mut g = self.decision_logs.write().await;
        let r = g.get_mut(key).map(|l| l.drop(id)).unwrap_or(false);
        drop(g);
        if r {
            self.persist_state().await;
        }
        r
    }

    /// Delta §11.12: arm a one-way prewalk. Injects the nudge into
    /// the transcript immediately; the first mutating tool call
    /// fires the model handoff. Idempotent: a second arm while a
    /// prewalk is armed is a no-op; a `Done` prewalk can be re-armed
    /// (a downhill walk).
    pub async fn arm_prewalk(&self, key: &str, target_model: impl Into<String>) {
        let target = target_model.into();
        let mut prewalk = kod_core_tools::prewalk::Prewalk::arm(target);
        // Inject the nudge as a user message and note its id, so the
        // fire step can splice it out by id.
        let msg_id = kod_types::MessageId::new();
        let nudge_id = msg_id.as_uuid().to_string();
        let nudge_msg = kod_types::ChatMessage::text(
            msg_id,
            kod_types::MessageRole::User,
            prewalk.nudge.clone(),
            time::OffsetDateTime::now_utc(),
        );
        prewalk.note_nudge_id(nudge_id);
        {
            let mut history = self.history.write().await;
            history.entry(key.to_string()).or_default().push(nudge_msg);
        }
        self.prewalks.write().await.insert(key.to_string(), prewalk);
    }

    /// Delta §11.12: whether a transcript has an armed prewalk.
    pub async fn prewalk_state(&self, key: &str) -> Option<kod_core_tools::prewalk::PrewalkState> {
        self.prewalks.read().await.get(key).map(|p| p.state.clone())
    }

    /// Delta §12.3: if the periodic memory-consolidation task has
    /// flagged a sharpshooter consolidation, run it now. Called at
    /// the top of every turn so the work happens on the engine's own
    /// async context (the periodic task holds only a router clone).
    pub(crate) async fn drain_due_sharpshooter(&self) {
        if self
            .sharpshooter_due
            .swap(false, std::sync::atomic::Ordering::Relaxed)
        {
            let files = self.consolidate_sharpshooter_now().await;
            if files > 0 {
                tracing::info!(
                    files,
                    "periodic sharpshooter consolidation wrote decisions files",
                );
            }
        }
    }

    /// Delta §11.12: check whether `tool_name` should fire the
    /// armed prewalk, and if so, run the handoff. Called from the
    /// tool loop after every tool call. Best-effort: a missing
    /// prewalk, an unarmed one, a read-only tool, or a done one
    /// each returns immediately.
    pub(crate) async fn maybe_fire_prewalk(&self, key: &str, tool_name: &str) {
        if !kod_core_tools::prewalk::Prewalk::is_mutating_tool(tool_name) {
            return;
        }
        // Take the prewalk out so a racing call does not double-fire.
        let taken = {
            let mut g = self.prewalks.write().await;
            match g.get(key) {
                Some(p) if p.is_armed() => g.remove(key),
                _ => None,
            }
        };
        let Some(mut prewalk) = taken else { return };

        // 1. Splice the nudge out of the transcript by id.
        if let Some(nudge_id) = prewalk.nudge_id.as_deref() {
            let mut history = self.history.write().await;
            if let Some(msgs) = history.get_mut(key) {
                msgs.retain(|m| m.id.as_uuid().to_string() != nudge_id);
            }
        }
        // 2. Switch the ephemeral model. The target is a display
        // string ("endpoint/model"); split on the first `/`.
        // A target with no `/` is taken as a model name on the
        // current endpoint (the caller's shorthand for "switch
        // models on the same endpoint").
        let model_ref = {
            let current = self.current_model.read().await.clone();
            match prewalk.target_model.split_once('/') {
                Some((endpoint, model)) => kod_provider::request::ModelRef::new(endpoint, model),
                None => kod_provider::request::ModelRef::new(
                    current.endpoint,
                    prewalk.target_model.clone(),
                ),
            }
        };
        *self.current_model.write().await = model_ref;
        // 3. Push the checklist as the next user message.
        {
            let mut history = self.history.write().await;
            history
                .entry(key.to_string())
                .or_default()
                .push(kod_types::ChatMessage::text(
                    kod_types::MessageId::new(),
                    kod_types::MessageRole::User,
                    prewalk.checklist.clone(),
                    time::OffsetDateTime::now_utc(),
                ));
        }
        prewalk.mark_done();
        // Keep the done state so a caller can see the switch happened.
        // (Read target_model before the insert moves prewalk.)
        let fired_target = prewalk.target_model.clone();
        self.prewalks.write().await.insert(key.to_string(), prewalk);
        tracing::info!(
            key,
            target = %fired_target,
            "prewalk fired: model switched mid-session",
        );
    }

    /// Delta §11.10: whether a transcript is in explicit plan mode.
    pub async fn is_in_plan_mode(&self, key: &str) -> bool {
        self.plan_mode.read().await.contains(key)
    }

    /// Delta §11.10: enter or exit plan mode for a transcript.
    /// Entering restricts the tool set to read-only tools; exiting
    /// restores the full set. The plan itself (a `Plan` value) is
    /// independent and persists across a mode toggle.
    pub async fn set_plan_mode(&self, key: &str, on: bool) {
        let mut guard = self.plan_mode.write().await;
        if on {
            guard.insert(key.to_string());
        } else {
            guard.remove(key);
        }
    }

    /// The plan for a transcript, if one has been created (Tier 2.1).
    pub async fn plan_for(&self, key: &str) -> Option<kod_core_state::plan::Plan> {
        self.plans.read().await.get(key).cloned()
    }

    /// Replace the plan for a transcript.
    pub async fn set_plan(&self, key: &str, plan: kod_core_state::plan::Plan) {
        self.plans.write().await.insert(key.to_string(), plan);
        self.persist_state().await;
    }

    /// Drop the plan for a transcript.
    pub async fn clear_plan(&self, key: &str) {
        self.plans.write().await.remove(key);
        self.plan_reference_paths.write().await.remove(key);
        self.persist_state().await;
    }

    /// Delta §11.10: declare a path whose `read_file` result must
    /// survive shake and prune for `key`. Replaces the set. Called
    /// by a caller that wants to protect a plan document, a design
    /// note, or any file the model needs to keep re-reading.
    pub async fn set_plan_reference_paths(
        &self,
        key: &str,
        paths: std::collections::HashSet<std::path::PathBuf>,
    ) {
        if paths.is_empty() {
            self.plan_reference_paths.write().await.remove(key);
        } else {
            self.plan_reference_paths
                .write()
                .await
                .insert(key.to_string(), paths);
        }
    }

    /// The currently-declared reference paths for `key`. Used by the
    /// mechanical-compaction path.
    pub async fn plan_reference_paths(
        &self,
        key: &str,
    ) -> std::collections::HashSet<std::path::PathBuf> {
        self.plan_reference_paths
            .read()
            .await
            .get(key)
            .cloned()
            .unwrap_or_default()
    }

    /// Apply a `PlanUpdate` to the transcript's plan, if one exists.
    /// Returns the human-readable description from `Plan::apply`, or
    /// a message saying no plan exists.
    pub async fn apply_plan_update(&self, key: &str, update: kod_core_state::plan::PlanUpdate) -> String {
        // Delta §11.10: a ReferencePath update changes both the plan's
        // own list and the engine's protected-path set. The set is
        // derived from the plan, so a caller reads a single source of
        // truth. Resolve relative paths against the transcript's
        // working dir (a swarm agent's worktree, otherwise the
        // engine root).
        let mut g = self.plans.write().await;
        let out = match g.get_mut(key) {
            Some(p) => p.apply(update),
            None => "No plan exists for this session. A plan is created                      on the first turn of a complex task."
                .to_string(),
        };
        let reference_paths: Option<Vec<String>> = g.get(key).map(|p| p.reference_paths.clone());
        drop(g);

        if let Some(paths) = reference_paths {
            let working_dir = self
                .transcript_working_dirs
                .read()
                .await
                .get(key)
                .cloned()
                .unwrap_or_else(|| self.working_dir.clone());
            let resolved: std::collections::HashSet<std::path::PathBuf> = paths
                .into_iter()
                .map(|p| {
                    let pb = std::path::PathBuf::from(&p);
                    if pb.is_absolute() {
                        pb
                    } else {
                        working_dir.join(pb)
                    }
                })
                .collect();
            self.set_plan_reference_paths(key, resolved).await;
        }
        out
    }

    /// Record a session-scoped learned allow for a tool call
    /// (Tier 2.3). Called by the approval overlay's "always" action.
    pub async fn learn_allow(&self, call: &ToolCall) {
        self.learned_allows
            .write()
            .await
            .insert(LearnedAllow::from_call(call));
    }

    /// Number of learned allows this session.
    pub async fn learned_allow_count(&self) -> usize {
        self.learned_allows.read().await.len()
    }

    /// Forget every learned allow.
    pub async fn clear_learned_allows(&self) {
        self.learned_allows.write().await.clear();
    }

    /// True when `call` matches a learned allow.
    pub(crate) async fn is_learned_allowed(&self, call: &ToolCall) -> bool {
        let key = LearnedAllow::from_call(call);
        self.learned_allows.read().await.contains(&key)
    }
}
