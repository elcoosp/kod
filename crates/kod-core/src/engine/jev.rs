use super::*;

impl KodEngine {
    /// Snapshot of the cache ledger for a `/cache` surface (P1).
    /// Returns (endpoint, cached_tokens, last_used_turn) plus the
    /// currently-warm endpoint's name.
    pub fn cache_snapshot(&self) -> (Vec<(String, u64, u64)>, Option<String>) {
        match self.cache_ledger.lock() {
            Ok(l) => (l.snapshot(), l.sticky_endpoint().map(str::to_string)),
            Err(_) => (Vec::new(), None),
        }
    }

    /// Snapshot of the endpoint circuit breaker (hygiene 3.2).
    /// Returns (endpoint, failures, last_error) for every endpoint
    /// currently in cooldown.
    pub fn unhealthy_endpoints(&self) -> Vec<(String, u32, Option<String>)> {
        match self.endpoint_health.lock() {
            Ok(h) => h
                .unhealthy()
                .into_iter()
                .map(|(name, fails, err)| (name, fails, err.map(str::to_string)))
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Consume the one-shot marker-suppression flag for `key`.
    ///
    /// Returns `true` exactly once after a tool-filter commit that
    /// changed the enabled set, `false` otherwise. `build_grounded_request`
    /// calls this to decide whether to set
    /// `CompletionRequest::cache_transcript`.
    ///
    /// Consuming (rather than reading) the flag means a turn with
    /// many rounds suppresses the marker only on the first — the
    /// prefix is stable from round 2 onward within the same turn.
    pub(crate) async fn consume_marker_suppression(&self, key: &str) -> bool {
        let mut states = self.tool_filter_states.write().await;
        match states.get_mut(key) {
            Some(state) if state.suppress_marker_once => {
                state.suppress_marker_once = false;
                true
            }
            _ => false,
        }
    }

    /// Derive the turn's sensitivity from its input's @-references
    /// (P7). A turn that mentions `.env`, a private key, or a path
    /// the policy engine read-protects is Sensitive; a turn that
    /// mentions only dotfiles is Internal; otherwise Public.
    ///
    /// Callers with richer knowledge (a TUI's file picker, a swarm
    /// subtask's brief) can call `set_sensitivity` directly after
    /// this to override.
    pub(crate) async fn update_sensitivity_from_input(&self, input: &str) {
        // Extract @-prefixed path tokens.
        let mut paths: Vec<std::path::PathBuf> = Vec::new();
        for tok in input.split_whitespace() {
            if let Some(rest) = tok.strip_prefix('@') {
                let p = rest.trim_matches(|c: char| {
                    !c.is_alphanumeric() && c != '/' && c != '.' && c != '_' && c != '-'
                });
                if !p.is_empty() {
                    paths.push(std::path::PathBuf::from(p));
                }
            }
        }
        // Read-protection globs come from the policy engine. None
        // installed means no protections, which the classifier
        // treats as "everything Public".
        // A path the policy engine read-protects (a `.env`, a
        // private key) is Sensitive. The `denied` closure is the
        // same predicate: kod has one read-protection mechanism, and
        // a path under it is the strongest signal available here.
        // A future "denied" list with different semantics would
        // thread a second closure through without changing the
        // classifier.
        let protected = match self.policy().await {
            Some(p) => p.read_protection().clone(),
            None => kod_config::ReadProtection::default(),
        };
        let s = kod_core_state::sensitivity::classify(
            &paths,
            |p| protected.matches(p),
            |p| protected.matches(p),
        );
        *self.current_sensitivity.write().await = s;
    }

    /// Refresh the `tool_search` inventory from the live registry.
    ///
    /// Called after registering a batch of tools or after an MCP
    /// server attaches. Cheap; the search reads the same
    /// `Arc<RwLock>` the engine writes.
    pub async fn refresh_tool_inventory(&self) {
        let defs = self.tools.get_definitions().await;
        if let Ok(mut inv) = self.tool_inventory.write() {
            *inv = kod_tools::tool_search::ToolInventory::from_definitions(defs);
        }
    }

    /// Set the current turn's sensitivity (P7). Callers set this
    /// before a prompt so the routing gate can filter endpoints by
    /// their declared trust tier.
    pub async fn set_sensitivity(&self, s: kod_core_state::sensitivity::Sensitivity) {
        *self.current_sensitivity.write().await = s;
    }

    /// The current turn's sensitivity.
    pub async fn current_sensitivity(&self) -> kod_core_state::sensitivity::Sensitivity {
        *self.current_sensitivity.read().await
    }

    pub(crate) fn next_turn_id(&self) -> kod_core_state::trace::TurnId {
        self.next_turn_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    /// Emit a completed trace, if a writer is installed. Failure to
    /// write is logged but never propagated — the trace is
    /// diagnostic, not functional.
    pub(crate) fn emit_turn_trace(&self, trace: &kod_core_state::trace::TurnTrace) {
        if let Ok(guard) = self.turn_trace_writer.read()
            && let Some(w) = guard.as_ref()
            && let Err(e) = w.record(trace)
        {
            tracing::warn!(
                error = %e,
                path = %w.path().display(),
                "could not append turn trace",
            );
        }
    }

    /// The session log path, when one is installed.
    pub fn session_log_path(&self) -> Option<std::path::PathBuf> {
        self.session_recorder
            .read()
            .ok()
            .and_then(|guard| guard.as_ref().map(|r| r.path().to_path_buf()))
    }

    /// Record the current user request for `key`. Called at the top
    /// of each `process*` entry point so deeper helpers can include
    /// the request in their Jev state. Idempotent — a caller that
    /// forgets to clear a previous request still sees the newest one.
    pub(crate) async fn set_current_request(&self, key: &str, input: &str) {
        self.current_requests
            .write()
            .await
            .insert(key.to_string(), input.to_string());
    }

    /// The active user request for `key`, if any.
    pub(crate) async fn current_request(&self, key: &str) -> Option<String> {
        self.current_requests.read().await.get(key).cloned()
    }

    /// Drop the request recorded for `key`. Called at the end of a
    /// `process*` call so a subsequent tool round on a stale key does
    /// not see the wrong request.
    pub(crate) async fn clear_current_request(&self, key: &str) {
        self.current_requests.write().await.remove(key);
    }

    /// Install a Jev client (design P0.1). Call sites that
    /// consult Jev check [`KodEngine::jev_client`] first and
    /// fall through to their heuristic when it is `None`.
    pub fn set_jev_client(&self, client: crate::jev::JevClient) {
        if let Ok(mut slot) = self.jev_client.write() {
            *slot = Some(std::sync::Arc::new(client));
        }
    }

    /// Install a scripted decider (P5.6 follow-up). Used by tests to
    /// inject deterministic verdicts without a network round-trip.
    pub fn set_jev_decider(&self, decider: std::sync::Arc<dyn crate::jev::JevDecider>) {
        if let Ok(mut slot) = self.jev_client.write() {
            *slot = Some(decider);
        }
    }

    /// Update a single Jev threshold and rebuild the client so the
    /// change takes effect immediately (P0.1 follow-up).
    ///
    /// Returns `Ok(())` on success. The caller decides whether to
    /// persist the new value to config — the engine only owns the
    /// in-process client.
    pub async fn update_jev_threshold(&self, name: &str, value: f32) -> Result<()> {
        let Some(client) = self.jev_client() else {
            return Err(KodError::InvalidState(
                "Jev is not enabled on this engine".to_string(),
            ));
        };
        let mut th = client.thresholds().clone();
        if !th.set(name, value) {
            return Err(KodError::InvalidParameters {
                reason: format!("unknown threshold name: {name}"),
            });
        }
        th.clamp();
        let new_client = client
            .with_thresholds(th)
            .map_err(|e| KodError::InvalidState(format!("rebuild JevClient: {e}")))?;
        if let Ok(mut slot) = self.jev_client.write() {
            *slot = Some(new_client);
        }
        Ok(())
    }

    /// The installed Jev client, if any. Cloned so the caller
    /// does not hold the lock across an await.
    pub fn jev_client(&self) -> Option<std::sync::Arc<dyn crate::jev::JevDecider>> {
        self.jev_client.read().ok().and_then(|g| g.clone())
    }

    /// True when a Jev client is installed on this engine.
    /// Cheap — used by `/jev` and by any UI that wants to
    /// indicate the integration is live.
    pub fn jev_enabled(&self) -> bool {
        self.jev_client.read().map(|g| g.is_some()).unwrap_or(false)
    }

    /// A short status line for `/jev`. `None` when no client is
    /// installed.
    pub fn jev_status(&self) -> Option<String> {
        let client = self.jev_client()?;
        let cfg = client.config();
        Some(format!(
            "enabled={} model={} cache_ttl={}s cache_entries={} timeout={}ms fail_open={} redact_paths={}",
            cfg.enabled,
            cfg.model.as_deref().unwrap_or("jev-latest"),
            cfg.cache_ttl_secs,
            client.cache_len(),
            cfg.timeout_ms,
            cfg.fail_open,
            cfg.redact_paths,
        ))
    }

    /// The active thresholds for `/jev`, formatted for display.
    pub fn jev_thresholds_line(&self) -> Option<String> {
        let client = self.jev_client()?;
        let t = client.thresholds();
        Some(format!(
            "task_classify={:.2} tool_filter={:.2} early_term={:.2} auto_approve={:.2} memory_filter={:.2} ambiguity={:.2}",
            t.task_classify_min,
            t.tool_filter_min,
            t.early_termination_min,
            t.auto_approve_min,
            t.memory_filter_min,
            t.ambiguity_min,
        ))
    }

    /// Drop every cached Jev decision. Returns the number of
    /// entries that were dropped, or `None` when no client is
    /// installed.
    pub fn jev_clear_cache(&self) -> Option<usize> {
        let client = self.jev_client()?;
        let n = client.cache_len();
        client.clear_cache();
        Some(n)
    }

    /// Ask Jev what kind of text a streamed chunk is (P1.4).
    ///
    /// Returns one of `prose_answer`, `reasoning`, `restatement`,
    /// `code_block`. The TUI routes `reasoning` and `restatement`
    /// into a collapsed row so the user perceives the model as
    /// faster without any actual latency change.
    ///
    /// Cheap: the state is a short buffer tail, the question is a
    /// four-label score. The TUI calls it at most once per second,
    /// not per chunk.
    ///
    /// Returns `None` when Jev is disabled or errored — the caller
    /// then renders the chunk as normal prose, which is the
    /// pre-Jev behaviour.
    /// Decide the sandbox mode for one `execute_command` (P3.4).
    ///
    /// The engine's global mode is the default. On a session with
    /// the sandbox in `Auto`, Jev is asked whether the command needs
    /// OS-level sandboxing. A `safe` / `network_risk` command runs
    /// unsandboxed (sandbox disabled for this one call); a
    /// `filesystem_risk` or `destructive` command keeps the
    /// configured mode. `Require` is never downgraded — a user who
    /// asked for mandatory sandboxing gets it.
    ///
    /// Fail-open: any error keeps the configured mode.
    pub(crate) async fn choose_sandbox_mode_for_command(
        &self,
        holder: &str,
        command: &str,
        configured: kod_tools::context::SandboxMode,
    ) -> kod_tools::context::SandboxMode {
        use kod_tools::context::SandboxMode;
        // Only `Auto` is negotiable: `Disabled` already means no
        // sandbox, and `Require` is a user-enforced guarantee.
        if !matches!(configured, SandboxMode::Auto) {
            return configured;
        }
        let Some(jev) = self.jev_client() else {
            return configured;
        };
        let state = crate::jev::build_state(command, &[]);
        let labels = &["safe", "network_risk", "filesystem_risk", "destructive"];
        let started = std::time::Instant::now();
        let decision = jev
            .evaluate_score(&state, "Command risk level", labels)
            .await;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let (level, source) = match decision {
            Ok(d) => (d.value, crate::jev::DecisionSource::Jev),
            Err(_) => (String::new(), crate::jev::DecisionSource::Heuristic),
        };
        // H-S13: never downgrade the sandbox on the basis of Jev's
        // verdict alone. Jev classifies *model-authored* text; a
        // prompt-injected command can steer its own classification.
        // The verdict is a necessary condition, not sufficient: the
        // command must ALSO match a conservative static allowlist of
        // read-only / non-destructive shapes. Anything else stays
        // sandboxed regardless of what Jev says.
        let static_ok = command_is_sandbox_downgrade_safe(command);
        let result = match level.as_str() {
            "safe" | "network_risk" if static_ok => SandboxMode::Disabled,
            _ => SandboxMode::Auto,
        };
        if !static_ok && matches!(level.as_str(), "safe" | "network_risk") {
            tracing::warn!(
                command_preview = %crate::jev::preview_chars(command, 120),
                "Jev said safe, but the command fails the static allowlist; \
                 keeping the sandbox on",
            );
        }
        self.log_jev_decision(
            holder,
            "sandbox_decision",
            command,
            "risk_level",
            serde_json::json!({
                "risk_level": level,
                "sandbox_mode": format!("{result:?}"),
            }),
            1.0,
            elapsed_ms,
            false,
            source,
        );
        result
    }

    /// Refine the router's skill match with Jev (P4.2).
    ///
    /// The router's substring matcher misses semantic matches — a
    /// request to "design a landing page" does not contain the
    /// substring "ui-ux" and so the `ui-ux-designer` skill loses to
    /// whatever named-skill happened to match lexically. This helper
    /// asks Jev to score each available skill against the request
    /// and returns the names of the skills that scored at or above
    /// `[jev.thresholds].memory_filter_min`.
    ///
    /// The union of the router's match and Jev's semantic score is
    /// returned, so a lexical match is never dropped. Bounded to
    /// `MAX_SKILLS` (a project with 100 skills still costs one
    /// round-trip).
    ///
    /// Returns `None` when Jev is disabled, the loaded skill set is
    /// empty, or the call errors. `None` means "keep the router's
    /// answer as-is".
    /// Rank grep / search_files results by Jev relevance (P2.3).
    ///
    /// When a search returns more than `MIN_RESULTS_TO_RANK` hits,
    /// ask Jev which ones best address the user's request, then drop
    /// the ones scored below `[jev.thresholds].memory_filter_min`.
    /// The tool's own caps still bound the size; this pass just
    /// removes hits that are syntactically matches but semantically
    /// noise.
    ///
    /// Returns `None` when:
    ///
    /// * Jev is disabled,
    /// * the search returned fewer than `MIN_RESULTS_TO_RANK` hits,
    /// * the result payload does not carry a `results` array,
    /// * every hit scored relevant, or Jev errored.
    ///
    /// The `results` key is what the tool writes; a caller that
    /// renames it must update this helper.
    /// Ask Jev to triage the hunks in a unified diff (P2.4) before
    /// the diff reaches the model's prompt block.
    ///
    /// Splits the diff by `@@` hunk headers, asks Jev to score each
    /// one's relevance to the user's request (`context_only`,
    /// `relevant`, `critical`), and replaces the `context_only`
    /// hunks with a one-line `… (N lines elided, context only)` note.
    /// The unified-diff framing (--- / +++ headers) is preserved so
    /// the model can still parse the result.
    ///
    /// Returns `None` when:
    ///
    /// * Jev is disabled or has no current request for this transcript,
    /// * the diff has fewer than 2 hunks (nothing to compress),
    /// * every hunk scores `relevant` or `critical`,
    /// * the whole call errors.
    ///
    /// Returning `None` leaves the original result untouched — the
    /// caller uses the same `ToolResult` it already had.
    /// Write a `SessionEntry::ToolOutcome` for a completed tool call
    /// when the call is "interesting" (P5.3): it took at least
    /// `MIN_JEV_CLASSIFY_MS`, or it failed. The gate keeps the
    /// classification cost off trivially fast read tools while
    /// preserving the signal on the calls that matter for
    /// `/debug tokens` and `/jev stats`.
    ///
    /// Fail-silent: any error or timeout writes nothing — the raw
    /// `ToolCall` entry is the ground truth; this is enrichment.
    /// Ask Jev whether the reply answers the user's request (P5.4).
    ///
    /// Returns a short advisory string to append to the reply when
    /// Jev is confident the reply is off-track (`answers_the_question
    /// < 0.5` AND the response is at least 200 chars long). Returns
    /// `None` for every other case, so a normal reply is unchanged.
    ///
    /// Not a hard gate — the reply is still delivered. The advisory
    /// tells the user *why* they might want to /regenerate, which is
    /// often more useful than a silent quality score.
    ///
    /// Logged as `JevDecision` with `purpose = "quality_gate"`.
    /// Refine the diagnostic baseline diff with Jev (P4.4).
    ///
    /// The syntactic `diag_key` comparison treats a line-shifted
    /// diagnostic as new — "unused variable `x` at line 42" and
    /// "unused variable `x` at line 45" hash differently, so the
    /// second counts as introduced by the write even when it moved
    /// because an earlier edit added lines. Jev reads the (file,
    /// code, message) triple and answers whether each "new"
    /// diagnostic is genuinely new or a shifted version of one in
    /// the baseline.
    ///
    /// The return value is the index set of diagnostics the caller
    /// should still treat as new. On disabled/errored Jev, every
    /// index is returned (the syntactic diff stands).
    ///
    /// Logged as a JevDecision with `purpose = "diagnostic_triage"`.
    /// Ask Jev whether each citation in `text` is substantiated by
    /// the cited location (P4.5). The syntactic check in
    /// `citations::check_and_annotate` verifies the file exists and
    /// the line is in range; this pass checks the stronger claim —
    /// that the cited line actually supports the prose.
    ///
    /// Returns `text` unchanged when Jev is disabled, there are no
    /// citations, or every citation scores `relevant`/`essential`.
    /// Otherwise returns `text + "\n\n<semantic block>"` naming
    /// the citations that did not pass.
    ///
    /// Fail-open: on Jev error, the original text is returned and
    /// the failure is logged at `Heuristic`.
    pub(crate) async fn semantic_verify_citations(&self, key: &str, text: &str) -> String {
        let Some(jev) = self.jev_client() else {
            return text.to_string();
        };
        let citations = kod_core_state::citations::extract_citations(text);
        if citations.is_empty() {
            return text.to_string();
        }
        // Bound the batch — a reply with 100 citations is pathological.
        const MAX_CITATIONS: usize = 10;
        let slice: Vec<_> = citations.iter().take(MAX_CITATIONS).collect();

        // Read each cited file and capture the surrounding line(s).
        // A file that cannot be read is skipped — the syntactic
        // checker already reported it.
        let mut questions: Vec<(String, String)> = Vec::with_capacity(slice.len());
        for (idx, c) in slice.iter().enumerate() {
            let abs = self.working_dir.join(&c.raw_path);
            // F2c-11: read off the async worker — this fn is awaited
            // on the turn path and the path is model-derived.
            let abs_for_read = abs.clone();
            let Ok(content) =
                tokio::task::spawn_blocking(move || std::fs::read_to_string(&abs_for_read))
                    .await
                    .unwrap_or_else(|_| Err(std::io::Error::other("read task panicked")))
            else {
                continue;
            };
            let mut lines_iter = content.lines();
            let line_text = if c.line == 0 {
                String::new()
            } else {
                lines_iter
                    .nth(c.line.saturating_sub(1) as usize)
                    .unwrap_or("")
                    .to_string()
            };
            let end = c.end_line.unwrap_or(c.line);
            let slice_text = if end > c.line {
                content
                    .lines()
                    .skip(c.line.saturating_sub(1) as usize)
                    .take((end - c.line + 1) as usize)
                    .collect::<Vec<_>>()
                    .join("\n")
            } else {
                line_text
            };
            if slice_text.trim().is_empty() {
                continue;
            }
            questions.push((
                format!("citation_{idx}"),
                format!(
                    "Does the code at {}:{} support the claim made about it? Code excerpt:\n{}",
                    c.raw_path, c.line, slice_text
                ),
            ));
        }
        if questions.is_empty() {
            return text.to_string();
        }

        let state = crate::jev::build_state(text, &[]);
        let started = std::time::Instant::now();
        let result = jev.evaluate_yes_no_batch(&state, &questions).await;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let (rows, source) = match result {
            Ok(r) => (r, crate::jev::DecisionSource::Jev),
            Err(_) => (Vec::new(), crate::jev::DecisionSource::Heuristic),
        };

        let threshold = jev.thresholds().memory_filter_min; // 0.7 default
        let mut failing: Vec<String> = Vec::new();
        let mut answers = serde_json::Map::new();
        for (id, p) in &rows {
            answers.insert(id.clone(), serde_json::json!(p));
            if *p < threshold
                && let Some(rest) = id.strip_prefix("citation_")
                && let Ok(idx) = rest.parse::<usize>()
                && let Some(c) = slice.get(idx)
            {
                failing.push(format!("{}:{}", c.raw_path, c.line));
            }
        }

        self.log_jev_decision(
            key,
            "citation_semantic",
            &crate::jev::preview_chars(text, 200),
            "per_citation_support",
            serde_json::Value::Object(answers),
            1.0,
            elapsed_ms,
            false,
            source,
        );

        if failing.is_empty() {
            return text.to_string();
        }
        let mut out = String::from(text);
        out.push_str("\n\n## Citation semantic check\n\n");
        out.push_str(
            "Jev could not confirm that the following cited lines support the claims made about them:\n",
        );
        for f in &failing {
            out.push_str(&format!("- {f}\n"));
        }
        out
    }

    /// Pre-detect ambiguous requests and, on a streaming call,
    /// prompt the user for clarification before the first LLM round
    /// (P3.3).
    ///
    /// Returns the input unchanged when:
    ///
    /// * Jev is disabled,
    /// * the request is trivially short (`< 10` chars) — nothing to
    ///   disambiguate,
    /// * `is_ambiguous` scores below `[jev.thresholds].ambiguity_min`,
    /// * the caller has no chunk channel (`process` non-streaming),
    /// * the user cancels or times out the clarification dialog.
    ///
    /// Returns `input + "\n\nAdditional clarification from user:
    /// <answer>"` when the user supplies one. Every path logs a
    /// `SessionEntry::JevDecision` with `purpose = "ambiguity"`.
    ///
    /// Fail-open: a Jev error logs `Heuristic` and returns the input
    /// unchanged.
    /// Group approval requests by logical change (P3.2).
    ///
    /// One ask per pending call; the answers group calls that Jev
    /// scores as belonging to the same logical change (`change_a`
    /// through `change_d`, or `standalone`). The caller uses the
    /// grouping to present a single dialog with a combined summary
    /// instead of N dialogs, each of which the user must click
    /// through.
    ///
    /// Returns a map from group label to the indices in the input
    /// slice. The caller renders one row per group.
    ///
    /// Returns an empty map when Jev is disabled or the batch has
    /// fewer than 2 items.
    /// Pre-extract handoff facts from a transcript (P4.8).
    ///
    /// Given the transcript's user+assistant messages, ask Jev three
    /// yes/no questions per message:
    ///
    /// * `is_decision` — does this message contain a durable decision?
    /// * `is_unfinished_task` — does this message describe an
    ///   unfinished task?
    /// * `is_file_reference` — does this message reference a file
    ///   path?
    ///
    /// The messages that score above threshold on any question are
    /// returned in their original order, tagged with which categories
    /// they matched. The `/handoff` command embeds this list in the
    /// LLM prompt so the model has a curated list of the durable
    /// facts to render, instead of having to re-read every turn.
    ///
    /// Bounded to `MAX_MESSAGES` messages. Returns an empty vec when
    /// Jev is disabled or errors, and the caller falls back to its
    /// current behaviour.
    /// Filter MCP tools before they reach the LLM (P5.1).
    ///
    /// An MCP filesystem server exposes ~10 tools; a GitHub server
    /// exposes ~20. When more than `MIN_MCP_TOOLS_TO_FILTER` MCP
    /// tools are registered on the engine, ask Jev which one should
    /// handle the current request and return only that one (plus a
    /// small safety margin of `KEEP_TOP_N`). When the registry has
    /// few or no MCP tools, returns the input unchanged.
    ///
    /// Fail-open: on any Jev error or a request with no current text,
    /// the input is returned unchanged.
    /// Ask Jev whether the session has moved to a new phase (P5.5).
    ///
    /// Returns `Some((old, new))` when Jev is confident the phase
    /// changed between the last N turns and now, where N is the
    /// last `PHASE_WINDOW` user+assistant messages. The labels are
    /// drawn from a fixed set (`exploring`, `coding`, `debugging`,
    /// `testing`, `refactoring`, `documenting`).
    ///
    /// The caller (`Event::ResponseComplete` in the TUI) uses a
    /// positive return to suggest `/handoff`.
    ///
    /// Returns `None` when Jev is disabled, the transcript is too
    /// short to judge, the phase is unchanged, or the confidence
    /// is below `[jev.thresholds].auto_approve_min`.
    /// Compress a large `read_file` result by dropping lines Jev
    /// judges irrelevant (P2.2).
    ///
    /// Only `read_file` benefits meaningfully: the other tools'
    /// output is already structured (grep results, diffs, JSON) and
    /// the caller's own caps already handle those. A read_file of
    /// 400 lines for a task that needs 20 is the common case this
    /// targets.
    ///
    /// Bounded to `MAX_LINES_TO_SCORE` lines. Every line is asked
    /// about in one batch. Lines scored above `[jev.thresholds]
    /// .memory_filter_min` are kept; dropped runs are replaced with
    /// a one-line `… N lines elided` marker so line numbers stay
    /// meaningful to a caller that wants them.
    ///
    /// Returns `None` when Jev is disabled, the result is not a
    /// `read_file` success, the content is shorter than
    /// `MIN_LINES_TO_COMPRESS`, or the call errors.
    /// Ask Jev which registered endpoint should serve a task
    /// (P5.2). Returns `None` when Jev is disabled, no registry is
    /// installed, or Jev fails — the caller falls back to the static
    /// `[llm.routing.by_task]` table.
    ///
    /// Public so the swarm runner can consult it for a capability
    /// before the per-round dispatch.
    /// Semantic swarm overlap check (P4.7). Given the parsed
    /// subtasks, ask Jev which pairs touch the same conceptual file
    /// even when their globs do not share a prefix. Returns a list
    /// of `(i, j)` index pairs the caller may want to serialize,
    /// or an empty vec when Jev is disabled or finds no semantic
    /// overlap.
    ///
    /// Bounded to `MAX_PAIRS` pairs (`{n choose 2}` of the first few
    /// subtasks) so a large swarm does not produce a pathological
    /// request.
    ///
    /// Uses one score question per candidate pair, joined in a
    /// single batch of yes/no.
    pub async fn semantic_overlap_check(
        &self,
        subtasks: &[(String, Vec<String>)],
    ) -> Vec<(usize, usize)> {
        const MAX_SUBTASKS: usize = 6;
        let Some(jev) = self.jev_client() else {
            return Vec::new();
        };
        if subtasks.len() < 2 {
            return Vec::new();
        }
        let slice = &subtasks[..subtasks.len().min(MAX_SUBTASKS)];
        // Build the state once: the descriptions plus each subtask's
        // declared globs.
        let mut ctx = String::from("Subtasks:\n");
        for (i, (desc, globs)) in slice.iter().enumerate() {
            ctx.push_str(&format!(
                "[{}] {} :: {}\n",
                i,
                crate::jev::preview_chars(desc, 200),
                globs.join(", ")
            ));
        }
        let state = crate::jev::build_state(&ctx, &[]);
        let mut questions: Vec<(String, String)> = Vec::new();
        for i in 0..slice.len() {
            for j in (i + 1)..slice.len() {
                questions.push((
                    format!("pair_{i}_{j}"),
                    format!(
                        "Do subtasks {i} and {j} touch the same conceptual file, even if their declared globs differ?"
                    ),
                ));
            }
        }
        let started = std::time::Instant::now();
        let rows = match jev.evaluate_yes_no_batch(&state, &questions).await {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let threshold = jev.thresholds().memory_filter_min;
        let mut out: Vec<(usize, usize)> = Vec::new();
        let mut answers = serde_json::Map::new();
        for (id, p) in &rows {
            answers.insert(id.clone(), serde_json::json!(p));
            if *p >= threshold
                && let Some(rest) = id.strip_prefix("pair_")
                && let Some((a, b)) = rest.split_once('_')
                && let (Ok(i), Ok(j)) = (a.parse::<usize>(), b.parse::<usize>())
            {
                out.push((i, j));
            }
        }
        self.log_jev_decision(
            DEFAULT_TRANSCRIPT_KEY,
            "swarm_overlap",
            &crate::jev::preview_chars(&ctx, 200),
            "colliding_pairs",
            serde_json::Value::Object(answers),
            1.0,
            elapsed_ms,
            false,
            crate::jev::DecisionSource::Jev,
        );
        out
    }

    /// Ask Jev which capability best fits a subtask description
    /// (P4.6). Returns the answer as a string label so the caller
    /// maps it back through `kod_swarm::Capability::from_str`. Returns
    /// `None` when Jev is disabled or errors — the caller's
    /// heuristic (`capability_for`) is the fallback.
    ///
    /// Public because `swarm_runner` is a different module and calls
    /// this through the `Arc<KodEngine>`.
    pub async fn validate_subtask_capability(&self, description: &str) -> Option<String> {
        let jev = self.jev_client()?;
        let state = crate::jev::build_state(description, &[]);
        let labels = &[
            "coding",
            "testing",
            "documentation",
            "code-review",
            "planning",
            "research",
            "debugging",
            "refactoring",
        ];
        let started = std::time::Instant::now();
        let decision = jev
            .evaluate_score(
                &state,
                "Which capability best describes this subtask?",
                labels,
            )
            .await
            .ok()?;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        self.log_jev_decision(
            DEFAULT_TRANSCRIPT_KEY,
            "swarm_capability",
            description,
            "capability",
            serde_json::json!({ "capability": decision.value }),
            decision.confidence,
            elapsed_ms,
            false,
            crate::jev::DecisionSource::Jev,
        );
        Some(decision.value)
    }

    /// Ask Jev whether a subtask's write-set globs plausibly match
    /// its description (P4.6). Returns `Some(true)` for "yes",
    /// `Some(false)` for a confident "no", `None` when Jev is
    /// disabled or errors.
    pub async fn validate_subtask_globs(
        &self,
        description: &str,
        globs: &[String],
    ) -> Option<bool> {
        if globs.is_empty() {
            return None;
        }
        let jev = self.jev_client()?;
        let state = crate::jev::build_state(
            &format!(
                "Subtask: {description}\nWrite-set globs: {}",
                globs.join(", ")
            ),
            &[],
        );
        let pairs = [(
            "globs_match".to_string(),
            "Do these file globs plausibly match the subtask description?".to_string(),
        )];
        let started = std::time::Instant::now();
        let rows = jev.evaluate_yes_no_batch(&state, &pairs).await.ok()?;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let p_yes = rows.first().map(|(_, p)| *p).unwrap_or(1.0);
        self.log_jev_decision(
            DEFAULT_TRANSCRIPT_KEY,
            "swarm_globs",
            description,
            "globs_match",
            serde_json::json!({ "globs_match": p_yes }),
            p_yes,
            elapsed_ms,
            false,
            crate::jev::DecisionSource::Jev,
        );
        let threshold = jev.thresholds().task_classify_min;
        Some(p_yes >= threshold)
    }

    /// Reweight a `PromptBudget` allocation based on Jev (P2.1).
    ///
    /// The base allocation is the fixed 50/20/20/10 split for
    /// history / skills / memory / repomap. This helper asks Jev
    /// three yes/no questions — does this round need repomap, full
    /// history, and skill instructions — and shifts the `remaining`
    /// budget between sections accordingly. The total is preserved:
    /// sections only trade, they never grow the prompt.
    ///
    /// A section that says "no" contributes its share to the
    /// sections that said "yes". `memory` always contributes when it
    /// exists (dropping it has a bigger cost than any tokens saved),
    /// so it does not get its own question.
    ///
    /// Returns `base` unchanged when Jev is disabled, errors, or the
    /// input is too short to classify.
    /// Filter a whole `MemoryContext` through Jev (P2.5). Applies
    /// `filter_memory_entries_with_jev` to both the working and the
    /// long-term halves, keyed by memory id, and rebuilds the
    /// context with the surviving entries.
    ///
    /// The `total_tokens` field is recomputed as the sum of the
    /// survivors' estimated lengths, so the prompt budget sees the
    /// post-filter number, not the pre-filter one.
    ///
    /// No-op when Jev is disabled, the context is empty, or the
    /// transcript has no current request (a caller that never ran a
    /// `process_*` entry point).
    /// Filter memory entries by Jev relevance (P4.1). Given a list
    /// of `(id, text)` pairs and the user's current request, returns
    /// the subset Jev scores as `relevant` or `essential`.
    ///
    /// The whole set is sent in one Score question so a large memory
    /// list costs one round-trip, not one per entry. Entries whose
    /// relevance falls below `[jev.thresholds].memory_filter_min` are
    /// dropped. Fail-open: on Jev failure, the original list is
    /// returned unchanged.
    /// Try to answer an `ask_user` question from the existing
    /// context (P3.5). Returns `Some(answer)` when Jev is confident
    /// the question can be answered from the request text plus a
    /// recent history excerpt; `None` when the user should be asked.
    ///
    /// The caller uses the returned answer as the tool result
    /// instead of emitting a question marker, so the model proceeds
    /// without interrupting the user. Logged as a JevDecision.
    pub(crate) async fn try_answer_question_from_context(
        &self,
        key: &str,
        question: &str,
    ) -> Option<String> {
        let jev = self.jev_client()?;
        let request = self.current_request(key).await?;
        // Recent history gives Jev enough state to answer "what file"
        // style questions. Bounded so the state stays cheap.
        let hist = self.render_history_for(key).await;
        let hist_tail: String = hist
            .chars()
            .rev()
            .take(2000)
            .collect::<String>()
            .chars()
            .rev()
            .collect();

        let state = crate::jev::build_state(
            &format!(
                "User request: {request}\n\nRecent conversation excerpt:\n{hist_tail}\n\nQuestion the model wants to ask: {question}"
            ),
            &[],
        );
        let pairs = [(
            "can_answer_from_context".to_string(),
            "Can this question be answered from the available context?".to_string(),
        )];
        let started = std::time::Instant::now();
        let result = jev.evaluate_yes_no_batch(&state, &pairs).await;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let (p_yes, source) = match result {
            Ok(rows) => (
                rows.first().map(|(_, p)| *p).unwrap_or(0.0),
                crate::jev::DecisionSource::Jev,
            ),
            Err(_) => (0.0, crate::jev::DecisionSource::Heuristic),
        };
        let threshold = jev.thresholds().ambiguity_min;
        self.log_jev_decision(
            key,
            "ask_user_check",
            question,
            "can_answer_from_context",
            serde_json::json!({ "can_answer_from_context": p_yes }),
            p_yes,
            elapsed_ms,
            false,
            source,
        );
        if p_yes < threshold {
            return None;
        }
        // A confident "yes" does not by itself produce an answer. If
        // the model asked the user for a string, the context has one;
        // re-ask with the same request as the "answer". This keeps
        // the wrapper's shape unchanged — one yes/no gate, then the
        // caller's existing flow.
        Some(format!(
            "(Jev answered from context — the request already specified this.) {request}"
        ))
    }

    /// Choose the endpoint for a streaming round (P1.3).
    ///
    /// On the first round of a turn, and on every round after a tool
    /// execution, asks Jev what kind of round this is (planning,
    /// tool_execution, synthesis, summary) and maps the answer to an
    /// endpoint via `[jev.round_routing]`. Returns `None` when Jev is
    /// disabled, the kind has no mapping, the mapping does not name a
    /// registered endpoint, or the state does not warrant a route.
    ///
    /// Fail-open: any error returns `None` and the caller keeps the
    /// chain-resolved endpoint for this round.
    pub(crate) async fn pick_round_endpoint(
        &self,
        key: &str,
        round_idx: usize,
        had_tool_results: bool,
    ) -> Option<ModelRef> {
        let jev = self.jev_client()?;
        let cfg = jev.config();
        if cfg.round_routing.is_empty() {
            return None;
        }
        let request_text = self.current_request(key).await?;

        // State distilled from the loop counters. Short so the
        // request stays cheap.
        let state = crate::jev::build_state(
            &format!("User request: {request_text}"),
            &[
                ("round_index", &round_idx.to_string()),
                ("tool_ran", if had_tool_results { "yes" } else { "no" }),
            ],
        );
        let labels = &["planning", "tool_execution", "synthesis", "summary"];
        let question = "What kind of round is this?";
        let started = std::time::Instant::now();
        let decision = jev.evaluate_score(&state, question, labels).await;
        let elapsed_ms = started.elapsed().as_millis() as u64;

        let (kind, source) = match decision {
            Ok(d) => (d.value, crate::jev::DecisionSource::Jev),
            Err(_) => (String::new(), crate::jev::DecisionSource::Heuristic),
        };
        let endpoint = if kind.is_empty() {
            None
        } else {
            cfg.endpoint_for_round(&kind).map(String::from)
        };

        // Resolve to a ModelRef only when the endpoint is registered.
        let result = match endpoint.as_deref() {
            Some(name) => {
                let registry = self.registry.read().await.clone();
                registry.and_then(|reg| {
                    reg.default_model(name)
                        .map(|model| ModelRef::new(name.to_string(), model))
                })
            }
            None => None,
        };

        let answers = serde_json::json!({
            "round_kind": kind,
            "endpoint": endpoint,
            "applied": result.is_some(),
        });
        self.log_jev_decision(
            key,
            "round_routing",
            &format!("round {round_idx}"),
            "round_kind",
            answers,
            1.0,
            elapsed_ms,
            false,
            source,
        );
        result
    }

    /// Should the stream be cut short? Called from `stream_round` on
    /// every `EARLY_TERM_CHECK_EVERY_CHUNKS`-th chunk after the
    /// accumulated response reaches `EARLY_TERM_MIN_CHARS`
    /// (P1.2).
    ///
    /// Asks two questions in one round-trip:
    ///
    /// * `is_complete`: does the accumulated response fully answer
    ///   the user's request?
    /// * `is_off_track`: has the model drifted from the request?
    ///
    /// Returns `true` when either clears the configured
    /// `[jev.thresholds].early_termination_min`. Logs a
    /// `SessionEntry::JevDecision` on every call — a false positive
    /// here (cutting a response short) is exactly the failure mode
    /// the log exists to diagnose.
    ///
    /// Fail-open: disabled or errored Jev returns `false` and the
    /// stream continues to the model's natural terminator.
    pub(crate) async fn should_early_terminate(
        &self,
        key: &str,
        accumulated: &str,
    ) -> EarlyTermination {
        let Some(jev) = self.jev_client() else {
            return EarlyTermination::None;
        };
        let Some(request) = self.current_request(key).await else {
            return EarlyTermination::None;
        };
        // Require at least one full sentence: a model that has
        // emitted only "Let me" is not done, however confident Jev
        // sounds about it.
        if !accumulated.contains(['.', '!', '?', '\n']) {
            return EarlyTermination::None;
        }

        let threshold = {
            let t = jev.thresholds().early_termination_min;
            // Use the configured value when it is sane; fall
            // back to the documented default when a bad config
            // produced zero (every response would terminate).
            if t > 0.0 { t } else { EARLY_TERM_DEFAULT_MIN }
        };
        let state = crate::jev::build_state(
            &format!("User request: {request}\nResponse so far: {accumulated}"),
            &[],
        );
        let pairs = [
            (
                "is_complete".to_string(),
                "Does the accumulated response fully answer the user's request?".to_string(),
            ),
            (
                "is_off_track".to_string(),
                "Has the model drifted away from the user's request?".to_string(),
            ),
        ];
        let started = std::time::Instant::now();
        let result = jev.evaluate_yes_no_batch(&state, &pairs).await;
        let elapsed_ms = started.elapsed().as_millis() as u64;

        let (complete_p, off_track_p, source) = match result {
            Ok(rows) => {
                let c = rows
                    .iter()
                    .find(|(k, _)| k == "is_complete")
                    .map(|(_, p)| *p)
                    .unwrap_or(0.0);
                let o = rows
                    .iter()
                    .find(|(k, _)| k == "is_off_track")
                    .map(|(_, p)| *p)
                    .unwrap_or(0.0);
                (c, o, crate::jev::DecisionSource::Jev)
            }
            Err(_) => (0.0, 0.0, crate::jev::DecisionSource::Heuristic),
        };

        // Off-track beats complete when both fire: cutting the
        // stream is the right local action, but so is signalling
        // the caller to try a different endpoint. A model that is
        // confidently wrong on the first try should not be trusted
        // to finish the sentence.
        let verdict = if off_track_p >= threshold {
            EarlyTermination::OffTrack
        } else if complete_p >= threshold {
            EarlyTermination::Complete
        } else {
            EarlyTermination::None
        };
        let answers = serde_json::json!({
            "is_complete": complete_p,
            "is_off_track": off_track_p,
            "verdict": match verdict {
                EarlyTermination::None => "none",
                EarlyTermination::Complete => "complete",
                EarlyTermination::OffTrack => "off_track",
            },
        });
        let conf = complete_p.max(off_track_p);
        self.log_jev_decision(
            key,
            "early_termination",
            &crate::jev::preview_chars(accumulated, 200),
            "is_complete,is_off_track",
            answers,
            conf,
            elapsed_ms,
            false,
            source,
        );
        verdict
    }

    /// Ask Jev whether a batch of `Ask`-decision tool calls is safe
    /// to auto-approve (P3.1).
    ///
    /// For each call, two questions are asked in one round-trip:
    ///
    /// * `likely_approved`: would the user almost certainly approve
    ///   this call?
    /// * `risk_level`: read_only | reversible | destructive |
    ///   irreversible.
    ///
    /// A call is auto-approved only when:
    ///
    /// * `likely_approved >= jev.thresholds.auto_approve_min`, AND
    /// * `risk_level` is not `destructive` or `irreversible`.
    ///
    /// The set of auto-approved call indices is returned. Every
    /// decision is logged as a `SessionEntry::JevDecision`.
    ///
    /// Fail-open: on any Jev error, an empty set is returned and the
    /// caller's existing dialog path runs unchanged.
    /// Pre-filter the tool inventory with Jev (P1.1).
    ///
    /// One yes/no question per [`kod_types::ToolCategory`] asks
    /// "does this request need a category of tool?". Categories with
    /// a probability at or above `[jev.thresholds].tool_filter_min`
    /// survive; every other category's definitions are dropped.
    ///
    /// Safety valve: a filter that would leave the tool list empty
    /// returns the full list unchanged — asking a model to act
    /// without a single tool is never the right answer. Likewise,
    /// when Jev is disabled or errors, the full list is returned
    /// (fail-open).
    ///
    /// Logs one `SessionEntry::JevDecision` per call.
    /// Ask Jev which `TaskType` best describes `input`, and merge
    /// that answer with the keyword heuristic's. The rule is:
    ///
    /// * Jev disabled            -> return the heuristic as-is.
    /// * Jev returns unknown     -> return the heuristic as-is.
    /// * Jev confidence < threshold (from `JevConfig::thresholds`) ->
    ///   return the heuristic as-is.
    /// * Otherwise               -> return Jev's answer.
    ///
    /// Every path logs a `SessionEntry::JevDecision` so `/jev stats`
    /// sees the call. The state is a one-line description of the
    /// request; it deliberately does not carry file contents.
    /// Write one `SessionEntry::JevDecision` to the installed
    /// session log. No-op when no recorder is installed. Every
    /// Jev-aware call site goes through this helper so the log
    /// shape stays uniform and `/jev stats` can rely on it.
    #[allow(clippy::too_many_arguments)]
    pub fn log_jev_decision(
        &self,
        holder: &str,
        purpose: &str,
        state_preview: &str,
        questions_summary: &str,
        answers: serde_json::Value,
        confidence: f32,
        latency_ms: u64,
        cached: bool,
        source: crate::jev::DecisionSource,
    ) {
        if let Ok(guard) = self.session_recorder.read()
            && let Some(rec) = guard.as_ref()
        {
            let entry = kod_core_state::session_log::SessionEntry::JevDecision {
                timestamp_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0),
                holder: holder.to_string(),
                purpose: purpose.to_string(),
                state_preview: crate::jev::preview_chars(state_preview, 200),
                questions_summary: questions_summary.to_string(),
                answers,
                confidence,
                latency_ms,
                cached,
                source: source.as_str().to_string(),
            };
            let _ = rec.record(&entry);
        }
    }
}

/// Main engine for KOD
/// H-S13: a conservative static allowlist for the Jev-driven sandbox
/// downgrade. A command that fails any of these tests keeps its
/// sandbox regardless of what Jev said, because Jev classified
/// model-authored text and a prompt injection can flip its own
/// verdict.
///
/// The check is structural, not textual:
///   - no redirection or pipe characters
///   - no shell chaining operators
///   - no command substitution
///   - the first token must be one of a small set of common
///     dev / query binaries
///   - no arguments that look like script injection (`eval`, `exec`,
///     `source`, `.`, `:`)
pub(crate) fn command_is_sandbox_downgrade_safe(command: &str) -> bool {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return false;
    }
    // Structural: any of these means we cannot cheaply reason about
    // what the command does.
    const UNSAFE_TOKENS: &[&str] = &[";", "&&", "||", "|", ">", "<", "`", "$(", "${", "\n", "\r"];
    if UNSAFE_TOKENS.iter().any(|t| trimmed.contains(t)) {
        return false;
    }
    // First whitespace-separated token, allowing a full path.
    let first = trimmed
        .split_whitespace()
        .next()
        .unwrap_or("")
        .rsplit('/')
        .next()
        .unwrap_or("");
    // A curated list of binaries that are read-only or trivially
    // auditable. `git` is on the list only with a read-only
    // subcommand (checked below).
    const ALLOW: &[&str] = &[
        "ls", "cat", "head", "tail", "grep", "find", "pwd", "which", "echo", "true", "false", "wc",
        "sort", "uniq", "diff", "file", "stat", "tree", "du", "df", "date", "env", "printenv",
        "id", "whoami", "cargo", "rustc", "rustup", "go", "gofmt", "python", "python3", "node",
        "npm", "npx", "tsc", "ruff", "pytest", "make", "cmake",
    ];
    if !ALLOW.iter().any(|b| b == &first) {
        // `git` needs the subcommand check.
        if first != "git" {
            return false;
        }
        let sub = trimmed
            .split_whitespace()
            .nth(1)
            .unwrap_or("")
            .rsplit('/')
            .next()
            .unwrap_or("");
        return matches!(
            sub,
            "status" | "diff" | "log" | "show" | "branch" | "remote" | "blame"
        );
    }
    // Reject a couple of argument shapes that are still unsafe even
    // when the first token is allowlisted.
    for tok in ["eval", "exec", "source"] {
        if trimmed.split_whitespace().any(|w| w == tok) {
            return false;
        }
    }
    true
}
