use super::*;

impl KodEngine {
    /// Ask the default transcript's running prompt to stop at the next
    /// round boundary.
    pub fn request_cancel(&self) {
        let _ = self.request_cancel_for(DEFAULT_TRANSCRIPT_KEY);
    }

    /// Ask a specific transcript's running prompt to stop. Used by the
    /// swarm runner's per-agent cancel (D4-D4) so cancelling one agent
    /// does not stop the whole team.
    /// Request a cancel and return the new fire epoch.
    ///
    /// The caller that may later clear (the swarm retry path) records
    /// the returned epoch and passes it to
    /// [`Self::clear_cancel_through`], so a cancel arriving in the
    /// interim is not erased.
    pub fn request_cancel_for(&self, key: &str) -> u64 {
        // H-E10: `cancels` is a parking_lot RwLock — synchronous, no
        // block_on on the async hot path.
        let mut g = self.cancels.write();
        let entry = g.entry(key.to_string()).or_insert((0, false));
        entry.0 = entry.0.wrapping_add(1);
        entry.1 = true;
        entry.0
    }

    /// Clear a previous cancel for the default transcript (called when
    /// a new prompt is dispatched).
    pub fn clear_cancel(&self) {
        self.clear_cancel_for(DEFAULT_TRANSCRIPT_KEY);
    }

    /// Clear a specific transcript's cancel flag.
    pub fn clear_cancel_for(&self, key: &str) {
        self.cancels.write().remove(key);
    }

    /// The current fire epoch for a transcript, without firing.
    ///
    /// Used by the swarm runner to record "what this attempt should
    /// clear through": it captures the epoch before the attempt, and
    /// on retry passes it to [`Self::clear_cancel_through`]. Zero when
    /// no cancel was ever requested for the key.
    pub fn cancel_epoch_for(&self, key: &str) -> u64 {
        self.cancels.read().get(key).map(|(e, _)| *e).unwrap_or(0)
    }

    /// Clear a transcript's cancel only if no newer fire has happened
    /// since `observed_epoch`.
    ///
    /// The swarm retry path: it records the epoch before an attempt,
    /// and on retry clears through that epoch. A cancel that arrived
    /// during the attempt bumped the epoch, so the clear is refused
    /// and the cancel survives — which is the whole point.
    pub fn clear_cancel_through(&self, key: &str, observed_epoch: u64) {
        let mut g = self.cancels.write();
        if let Some(entry) = g.get_mut(key)
            && entry.0 <= observed_epoch
        {
            g.remove(key);
        }
    }

    /// True if a cancel was requested for the default transcript.
    pub fn is_cancelled(&self) -> bool {
        self.is_cancelled_for(DEFAULT_TRANSCRIPT_KEY)
    }

    /// Delta §9.8: pause the process-wide gate.
    ///
    /// In-flight streams run to completion (they are not checked
    /// mid-stream); the next model call and the next tool round
    /// park at their respective boundaries until `resume`. A run
    /// that is aborted while paused stays paused — the abort
    /// releases the run, not the gate.
    pub fn pause(&self) {
        self.pause_gate.pause();
    }

    /// Delta §9.8: resume the process-wide gate. Idempotent.
    pub fn resume(&self) {
        self.pause_gate.resume();
    }

    /// Whether the gate is currently paused, for a UI readout.
    /// Cheap (an atomic load); a caller that checks this and then
    /// proceeds has a race between the check and the proceed —
    /// call `wait_if_paused` at a boundary instead.
    pub fn is_paused(&self) -> bool {
        self.pause_gate.is_paused()
    }

    /// True if a cancel was requested for `key`.
    pub fn is_cancelled_for(&self, key: &str) -> bool {
        self.cancels
            .read()
            .get(key)
            .is_some_and(|(_, fired)| *fired)
    }

    /// H-RL1: the longest provider-suggested rate-limit window the
    /// engine may sleep out before re-driving a failed request.
    pub fn rate_limit_wait_budget(&self) -> std::time::Duration {
        std::time::Duration::from_secs(
            self.rate_limit_wait_budget_secs
                .load(std::sync::atomic::Ordering::Relaxed),
        )
    }

    /// H-RL1: install the rate-limit wait budget (seconds). Called by
    /// the CLI/TUI bootstrap from the endpoint's `rate_limit_wait_secs`
    /// (`None` → [`kod_config::DEFAULT_RATE_LIMIT_WAIT_SECS`]); `0`
    /// restores the legacy fail-fast behavior where a rate limit
    /// surfaces to the user immediately.
    pub fn set_rate_limit_wait_budget(&self, secs: u64) {
        self.rate_limit_wait_budget_secs
            .store(secs, std::sync::atomic::Ordering::Relaxed);
    }

    /// H-RL1: the waitable rate-limit window implied by `err`, when it
    /// is a rate limit at or under the engine's wait budget. `None`
    /// for anything else, for a zero hint, or when the budget is
    /// disabled (`0`). Shared by the streaming and collected retry
    /// paths; the typed `RateLimited`/`ServerBusy` variants are matched
    /// first and the string classifier only catches providers that
    /// surface the 429/503 as a formatted message.
    pub(crate) fn rate_limit_hint_within_budget(
        &self,
        err: &KodError,
    ) -> Option<std::time::Duration> {
        let hint_secs = match err {
            KodError::RateLimited { retry_after_secs } => *retry_after_secs,
            KodError::ServerBusy { retry_after_secs } => *retry_after_secs,
            other => {
                match kod_core_routing::retry_strategy::TurnFailure::classify(&other.to_string()) {
                    kod_core_routing::retry_strategy::TurnFailure::TransportRateLimit {
                        retry_after_secs,
                    } => retry_after_secs?,
                    kod_core_routing::retry_strategy::TurnFailure::TransportServerBusy {
                        retry_after_secs,
                    } => retry_after_secs?,
                    _ => return None,
                }
            }
        };
        if hint_secs == 0 {
            return None;
        }
        let hint = std::time::Duration::from_secs(hint_secs);
        let budget = self.rate_limit_wait_budget();
        if budget.is_zero() || hint > budget {
            return None;
        }
        Some(hint)
    }

    /// H-RL1: sleep out a provider rate-limit window and tell the
    /// caller to re-drive the same request. Emits the
    /// [`RATE_LIMIT_WAIT_MARKER`] (or [`SERVER_BUSY_WAIT_MARKER`] for
    /// overload) so the TUI/CLI show a bounded,
    /// cancellable wait instead of a frozen spinner, then sleeps in
    /// one-second slices so `Esc` cancels promptly.
    ///
    /// Returns `Ok(Some(()))` when the caller should retry the same
    /// endpoint, `Ok(None)` when the error is not waitable (not a rate
    /// limit, hint above the budget, or the retry cap is spent) and
    /// the original error should propagate, and `Err` when the user
    /// cancelled during the wait.
    pub(crate) async fn rate_limit_retry_wait(
        &self,
        err: &KodError,
        attempt: u32,
        holder: &str,
        chunk_tx: &tokio::sync::mpsc::Sender<String>,
    ) -> Result<Option<()>> {
        let busy = matches!(err, KodError::ServerBusy { .. })
            || matches!(
                kod_core_routing::retry_strategy::TurnFailure::classify(&err.to_string()),
                kod_core_routing::retry_strategy::TurnFailure::TransportServerBusy { .. }
            );
        // Overload gets four waits per turn, rate limits two.
        let cap = if busy {
            MAX_SERVER_BUSY_RETRIES
        } else {
            MAX_RATE_LIMIT_RETRIES
        };
        if attempt >= cap {
            return Ok(None);
        }
        let Some(hint) = self.rate_limit_hint_within_budget(err) else {
            return Ok(None);
        };
        let marker = if busy {
            server_busy_wait_marker(hint.as_secs(), attempt + 1, cap)
        } else {
            rate_limit_wait_marker(hint.as_secs(), attempt + 1, cap)
        };
        let _ = chunk_tx.send(marker).await;
        if busy {
            tracing::warn!(
                holder = %holder,
                hint_secs = hint.as_secs(),
                attempt = attempt + 1,
                max = cap,
                "provider overloaded; waiting out the window and re-driving the turn"
            );
        } else {
            tracing::warn!(
                holder = %holder,
                hint_secs = hint.as_secs(),
                attempt = attempt + 1,
                max = cap,
                "provider rate limit; waiting out the window and re-driving the turn"
            );
        }
        let mut waited = std::time::Duration::ZERO;
        while waited < hint {
            if self.is_cancelled_for(holder) {
                return Err(KodError::InvalidState("cancelled by user".to_string()));
            }
            let step = std::time::Duration::from_secs(1).min(hint - waited);
            tokio::time::sleep(step).await;
            waited += step;
        }
        Ok(Some(()))
    }

    /// Delta §7.7 item 5: on a clean-stop turn (no tool calls), lift a
    /// `*** Begin Patch` envelope out of the reply text and run it as
    /// synthetic `patch_file` calls.
    ///
    /// A model that learned the OpenAI `apply_patch` envelope sometimes
    /// emits one as prose and stops — without this the edit is lost.
    /// The synthetic calls go through the normal tool path (approval,
    /// path lock, policy), so extraction is not an approval bypass.
    ///
    /// Returns `Some(note)` when at least one patch was recovered (the
    /// note describes what happened, for a system row), `None` when
    /// there was nothing to recover or every patch was rejected before
    /// dispatch.
    pub(crate) async fn maybe_recover_inline_patch(
        &self,
        holder: &str,
        final_text: &str,
        chunk_tx: Option<&tokio::sync::mpsc::Sender<String>>,
    ) -> Option<String> {
        let patches = kod_tools::patch_text::extract(final_text)?;
        if patches.is_empty() {
            return None;
        }
        let working_dir = self.working_dir_for(holder).await;
        let mut calls: Vec<ToolCall> = Vec::new();
        let mut notes: Vec<String> = Vec::new();
        for patch in &patches {
            let abs_of = |path: &str| {
                if std::path::Path::new(path).is_absolute() {
                    std::path::PathBuf::from(path)
                } else {
                    working_dir.join(path)
                }
            };
            let (path, original) = match patch {
                // W1: an Add onto a file that already has content would
                // duplicate it — `apply_unified_diff` splices the new
                // content at the top and keeps everything behind it, and
                // reports success. Skip with a note; a missing or empty
                // file is a genuine creation.
                kod_tools::patch_text::FilePatch::Add { path, .. } => {
                    match std::fs::read_to_string(abs_of(path)) {
                        Ok(body) if !body.is_empty() => {
                            notes.push(format!(
                                "  · {path}: skipped — file already exists with \
                                 content; an Add would duplicate it, use an \
                                 Update hunk"
                            ));
                            continue;
                        }
                        _ => (path.clone(), String::new()),
                    }
                }
                // W2: Delete cannot be expressed as a content diff.
                // Pre-fix it was folded into Update, so "deleting" a
                // file either truncated it to "\n" or (when the pre-read
                // failed) applied an empty no-op diff and reported
                // success. Deletion is a shell operation.
                kod_tools::patch_text::FilePatch::Delete { path } => {
                    notes.push(format!(
                        "  · {path}: skipped — file deletion is not supported \
                         by patch_file; remove it with a shell command"
                    ));
                    continue;
                }
                // W2 (second half): an unreadable Update source is
                // reported, not silently diffed against "".
                kod_tools::patch_text::FilePatch::Update { path, .. } => {
                    match std::fs::read_to_string(abs_of(path)) {
                        Ok(body) => (path.clone(), body),
                        Err(e) => {
                            notes.push(format!(
                                "  · {path}: skipped — could not read the file: {e}"
                            ));
                            continue;
                        }
                    }
                }
            };
            let diff = match kod_tools::patch_text::to_unified_diff(patch, &original) {
                Ok(d) => d,
                Err(e) => {
                    notes.push(format!("  · {path}: skipped — {e}"));
                    continue;
                }
            };
            calls.push(ToolCall {
                id: None,
                tool_name: "patch_file".to_string(),
                arguments: serde_json::json!({ "path": path, "patch": diff }),
            });
        }
        if calls.is_empty() {
            return Some(format!(
                "Recovered a `*** Begin Patch` block, but no hunk matched uniquely:\n{}",
                notes.join("\n")
            ));
        }
        let round = self.run_tool_calls(&calls, holder, chunk_tx).await;
        // W11: `applied` must exclude the rejected calls. Pre-fix it was
        // `calls.len()`, so a round with 3 calls and 1 failure reported
        // "3 file patch(es) … (1 rejected)" — the model was told its
        // failed patch had been applied.
        let failed: usize = round
            .results
            .iter()
            .filter(|r| matches!(r, kod_types::ToolResult::Error(_)))
            .count();
        let applied = calls.len().saturating_sub(failed);
        let mut note = format!(
            "Recovered {} file patch(es) from an inline `*** Begin Patch` block; \
             {applied} applied, {failed} rejected.",
            calls.len(),
        );
        if !notes.is_empty() {
            note.push('\n');
            note.push_str(&notes.join("\n"));
        }
        Some(note)
    }

    /// Queue a steering note on the default transcript.
    pub async fn steer(&self, note: &str) {
        self.steer_for(DEFAULT_TRANSCRIPT_KEY, note).await;
    }

    /// Queue a steering note on `key`. Injected into that transcript's
    /// conversation after the current tool round finishes.
    pub async fn steer_for(&self, key: &str, note: &str) {
        self.steer_interrupt_for(key, kod_core_state::steer::SoftInterrupt::user(note))
            .await;
    }

    /// Queue a typed soft interrupt on `key`.
    ///
    /// The typed entry point for producers that are not the user:
    /// swarm conflict notices, background-task completions, harness
    /// system messages. [`steer_for`] is the user-facing wrapper.
    ///
    /// An empty body is dropped — an interrupt with no content would
    /// render a bare header into the transcript, which is noise the
    /// model has to read past.
    pub async fn steer_interrupt_for(
        &self,
        key: &str,
        interrupt: kod_core_state::steer::SoftInterrupt,
    ) {
        if interrupt.content.trim().is_empty() {
            return;
        }
        let mut guard = self.steers.write().await;
        guard.entry(key.to_string()).or_default().push(interrupt);
    }

    /// Drain queued steer notes for `key` (each is applied once, in
    /// order).
    /// The soft interrupts currently queued for `key`, without
    /// draining. A caller that wants to display or test what is
    /// pending reads here; the round-boundary drain is
    /// [`Self::take_steers_for`].
    pub async fn pending_steers_for(&self, key: &str) -> Vec<kod_core_state::steer::SoftInterrupt> {
        self.steers
            .read()
            .await
            .get(key)
            .cloned()
            .unwrap_or_default()
    }

    pub(crate) async fn take_steers_for(
        &self,
        key: &str,
    ) -> Vec<kod_core_state::steer::SoftInterrupt> {
        let mut guard = self.steers.write().await;
        guard.remove(key).unwrap_or_default()
    }
}
