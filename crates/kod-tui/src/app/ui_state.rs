//! Small UI state on `KodApp`: theme, phase, header/status accessors,
//! approval/confirmation dialogs, and export helpers.
//!
//! Extracted from `app/mod.rs` (S9).

use super::*;

impl KodApp {
    pub fn phase(&self) -> &GenPhase {
        &self.phase
    }

    pub(super) fn set_phase(&mut self, phase: GenPhase) {
        self.phase = phase;
    }

    /// Short human label: `connecting`, `generating`, `tool: cargo test`, `thinking`.
    /// No timers — elapsed was noisy and duplicated across header/status.
    pub fn phase_label(&self) -> Option<String> {
        if !self.generating {
            return None;
        }
        let label = match &self.phase {
            GenPhase::Idle | GenPhase::Connecting => "connecting…".to_string(),
            GenPhase::Generating => "thinking…".to_string(),
            GenPhase::ExecutingTool(t) => {
                let short = t.chars().take(28).collect::<String>();
                format!("tool: {short}")
            }
            GenPhase::Summarizing => "thinking…".to_string(),
            // Engine-reported auxiliary work — show what the turn is
            // actually doing instead of a stale "thinking…".
            GenPhase::Activity(l) => l.chars().take(40).collect::<String>(),
            // Live countdown from the wait deadline the marker
            // carried: a 20-minute silent wait looks identical to a
            // stuck one without it.
            GenPhase::RateLimited => match self.rate_limit_deadline {
                Some(deadline) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        "rate-limited — retrying…".to_string()
                    } else {
                        format!(
                            "rate-limited — retry in {}…",
                            kod_core::engine::format_duration_ms(
                                remaining.as_millis().min(u128::from(u64::MAX)) as u64
                            )
                        )
                    }
                }
                None => "rate-limited — waiting…".to_string(),
            },
        };
        Some(label)
    }

    /// Enter an engine-reported auxiliary phase (Jev verdict, memory
    /// fact extraction, decision mining). Overwritten by the next
    /// text / tool / thinking / turn signal; a no-op when idle so a
    /// stray marker can never light the spinner on its own.
    pub fn begin_activity(&mut self, label: &str) {
        if self.generating {
            let label = label.trim();
            if !label.is_empty() {
                self.set_phase(GenPhase::Activity(label.to_string()));
            }
        }
    }

    /// Enter the rate-limit wait: the provider refused the turn and
    /// the engine is sleeping out the retry window. Replaces the
    /// misleading "connecting…" the spinner showed while waiting.
    /// Records the deadline so the spinner counts down live.
    pub fn begin_rate_limit_wait(&mut self, wait_secs: u64) {
        if self.generating {
            self.rate_limit_deadline =
                Some(Instant::now() + std::time::Duration::from_secs(wait_secs));
            self.set_phase(GenPhase::RateLimited);
        }
    }

    /// Enter the post-tool thinking phase (tool result reinjected, LLM
    /// is reasoning again). Shows "thinking…" not the stale tool line.
    pub fn begin_thinking(&mut self) {
        if self.generating {
            self.current_tool = None;
            self.set_phase(GenPhase::Summarizing);
        }
    }

    pub fn last_prompt(&self) -> Option<&str> {
        self.last_prompt.as_deref()
    }

    pub fn set_last_prompt(&mut self, prompt: &str) {
        self.last_prompt = Some(prompt.to_string());
    }

    /// True when recent generations keep failing (drives the offline badge).
    pub fn is_offline(&self) -> bool {
        self.fail_count >= 2
    }

    pub fn fail_count(&self) -> usize {
        self.fail_count
    }

    pub fn theme(&self) -> &Theme {
        &self.theme
    }

    pub fn set_theme(&mut self, theme: Theme) {
        self.theme = theme;
    }

    /// Set the theme by name, returning `true` if the name matches a
    /// built-in theme (currently `dark` and `light`) and `false`
    /// otherwise. On `false` the theme is set to dark — the same
    /// fallback `Theme::from_name` has always applied — but the caller
    /// can now tell the user that the name was not recognized instead
    /// of printing a transition into a theme that does not exist.
    ///
    /// Previously, `/theme neon` printed "Theme dark → neon" while the
    /// palette was in fact dark; a small lie, but the whole point of
    /// a theme command is to see what you typed take effect.
    pub fn try_set_theme(&mut self, name: &str) -> bool {
        let lower = name.trim().to_ascii_lowercase();
        match lower.as_str() {
            "dark" => {
                self.theme = Theme::dark();
                true
            }
            "light" => {
                self.theme = Theme::light();
                true
            }
            _ => {
                self.theme = Theme::dark();
                false
            }
        }
    }

    /// Cycle dark → light → dark. Returns the new theme name.
    pub fn cycle_theme(&mut self) -> String {
        let next = if self.theme.name == "light" {
            Theme::dark()
        } else {
            Theme::light()
        };
        let name = next.name.clone();
        self.theme = next;
        name
    }

    pub fn should_quit(&self) -> bool {
        self.should_quit
    }

    pub fn quit(&mut self) {
        self.should_quit = true;
    }

    pub fn set_should_quit(&mut self, quit: bool) {
        self.should_quit = quit;
    }

    /// Copy the last assistant reply to the system clipboard (the `y` key).
    pub fn copy_last_to_clipboard(&self) -> bool {
        let Some(text) = self.last_assistant_text() else {
            return false;
        };
        crate::clipboard::write_clipboard(text)
    }

    /// Set the list of models the provider offers (for `/model` completion).
    pub fn set_available_models(&mut self, models: Vec<String>) {
        self.available_models = models;
    }

    /// The models the provider last reported. Empty when no list has
    /// been fetched yet (engine not initialized, list_models failed).
    /// Callers that want to validate a `/model <name>` request must
    /// treat empty as "unknown — do not warn" rather than "no models
    /// exist", because an empty list is also what a cold provider
    /// returns before the first list_models call.
    pub fn available_models(&self) -> &[String] {
        &self.available_models
    }

    pub fn set_loaded_skills(&mut self, names: Vec<String>) {
        self.loaded_skills = names;
    }

    pub fn loaded_skills(&self) -> &[String] {
        &self.loaded_skills
    }

    pub fn set_notify_bell(&mut self, enabled: bool) {
        self.notify_bell_enabled = enabled;
    }

    pub fn notify_bell(&self) -> bool {
        self.notify_bell_enabled
    }

    pub fn set_autocompact(&mut self, enabled: bool) {
        self.autocompact_enabled = enabled;
    }

    pub fn autocompact(&self) -> bool {
        self.autocompact_enabled
    }

    pub fn set_session_system_prompt(&mut self, text: String) {
        self.session_system_prompt = Some(text);
    }

    pub fn clear_session_system_prompt(&mut self) {
        self.session_system_prompt = None;
    }

    pub fn session_system_prompt(&self) -> Option<&str> {
        self.session_system_prompt.as_deref()
    }

    /// If the turn that just finished ran longer than `threshold`, ring
    /// the terminal bell and (on supporting terminals) send an OSC 9
    /// notification. Returns the elapsed time when a bell fired, so
    /// the caller can print an informational line.
    ///
    /// Bells are cheap and silent on most setups; a threshold keeps
    /// them from firing on every trivial turn. 30 seconds is
    /// conservative — a user can react, but not every keystroke gets a
    /// beep.
    pub fn notify_turn_complete(
        &mut self,
        threshold: std::time::Duration,
    ) -> Option<std::time::Duration> {
        if !self.notify_bell_enabled {
            // Still clear the timestamp so the state does not leak
            // into the next turn's measurement.
            self.turn_started_at = None;
            return None;
        }
        let started = self.turn_started_at.take()?;
        let elapsed = started.elapsed();
        if elapsed < threshold {
            return None;
        }
        // Terminal bell.
        eprint!("\x07");
        // OSC 9 notification (iTerm2, Windows Terminal, kitty). Some
        // terminals do not implement it and ignore the sequence.
        let msg = format!("kod: turn completed in {}s", elapsed.as_secs());
        eprint!("\x1b]9;{}\x07", msg);
        Some(elapsed)
    }

    /// Render the current chat as a self-contained HTML document with
    /// inline CSS. No external assets, no scripts — the output is a
    /// single file a user can email or drop in a wiki.
    pub fn export_html(&self) -> String {
        let mut out = String::with_capacity(4096);
        out.push_str("<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">");
        out.push_str("<title>KOD session</title>");
        out.push_str("<style>");
        out.push_str("body{font-family:system-ui,sans-serif;max-width:900px;margin:2rem auto;padding:0 1rem;line-height:1.5;color:#111;background:#fff}");
        out.push_str(".msg{margin:1.5rem 0;padding:.75rem 1rem;border-left:3px solid #ddd;background:#fafafa}");
        out.push_str(".you{border-left-color:#3b82f6}");
        out.push_str(".ai{border-left-color:#10b981}");
        out.push_str(".sys{border-left-color:#f59e0b;color:#555}");
        out.push_str(
            ".tool{border-left-color:#a855f7;font-family:ui-monospace,monospace;font-size:.9em}",
        );
        out.push_str(".agent{border-left-color:#ec4899}");
        out.push_str(".role{font-size:.8em;text-transform:uppercase;letter-spacing:.05em;color:#666;margin-bottom:.25rem}");
        out.push_str(".time{font-size:.75em;color:#999;margin-left:.5rem}");
        out.push_str("pre{white-space:pre-wrap;word-wrap:break-word;margin:0}");
        out.push_str("</style></head><body>\n");
        out.push_str("<h1>KOD session</h1>\n");

        let mut ordered: Vec<&Message> = self.messages.iter().collect();
        ordered.sort_by_key(|m| m.sequence);
        for m in ordered {
            let (cls, label) = match &m.role {
                kod_types::MessageRole::User => ("you", "you"),
                kod_types::MessageRole::Assistant => ("ai", "ai"),
                kod_types::MessageRole::System => ("sys", "sys"),
                kod_types::MessageRole::Tool => ("tool", "tool"),
                kod_types::MessageRole::Agent(_) => ("agent", "agent"),
            };
            let escaped = html_escape(&m.content);
            out.push_str(&format!(
                "<div class=\"msg {}\"><div class=\"role\">{}<span class=\"time\">{}</span></div><pre>{}</pre></div>\n",
                cls,
                label,
                m.timestamp.format("%Y-%m-%d %H:%M:%S"),
                escaped,
            ));
        }
        out.push_str("</body></html>\n");
        out
    }

    /// Render the current chat as Markdown. Same shape the CLI's
    /// `kod sessions export --format markdown` produces, so the two
    /// paths agree.
    pub fn export_markdown(&self) -> String {
        let mut out = String::from("# KOD session\n\n");
        let mut ordered: Vec<&Message> = self.messages.iter().collect();
        ordered.sort_by_key(|m| m.sequence);
        for m in ordered {
            let label = match &m.role {
                kod_types::MessageRole::User => "you",
                kod_types::MessageRole::Assistant => "ai",
                kod_types::MessageRole::System => "sys",
                kod_types::MessageRole::Tool => "tool",
                kod_types::MessageRole::Agent(_) => "agent",
            };
            out.push_str(&format!(
                "## {} · {}\n\n",
                label,
                m.timestamp.format("%Y-%m-%d %H:%M:%S")
            ));
            let fenced = matches!(
                m.role,
                kod_types::MessageRole::Assistant | kod_types::MessageRole::Tool
            );
            if fenced {
                out.push_str("```\n");
                out.push_str(&m.content);
                if !m.content.ends_with('\n') {
                    out.push('\n');
                }
                out.push_str("```\n\n");
            } else {
                out.push_str(&m.content);
                out.push_str("\n\n");
            }
        }
        out
    }

    /// Add a file to the pending attachment list. Returns false when
    /// the file cannot be read — the caller can then print a message.
    pub fn attach_file(&mut self, path: std::path::PathBuf) -> bool {
        if !path.is_file() {
            return false;
        }
        if self.attached_files.contains(&path) {
            return true; // idempotent
        }
        self.attached_files.push(path);
        true
    }

    /// Take and clear the attachments, returning the file list.
    pub fn take_attachments(&mut self) -> Vec<std::path::PathBuf> {
        std::mem::take(&mut self.attached_files)
    }

    /// The currently attached files (for display).
    pub fn attached_files(&self) -> &[std::path::PathBuf] {
        &self.attached_files
    }

    /// Clear all attachments.
    pub fn clear_attachments(&mut self) {
        self.attached_files.clear();
    }

    /// Help overlay visibility. `AppMode::Help` (legacy `h`/`?` path) also
    /// counts so both spellings open the same overlay.
    pub fn show_help(&self) -> bool {
        self.show_help || matches!(self.mode, AppMode::Help)
    }

    /// Toggle the help overlay. Closing also leaves `AppMode::Help`.
    pub fn toggle_help(&mut self) {
        if self.show_help() {
            self.show_help = false;
            if matches!(self.mode, AppMode::Help) {
                self.mode = AppMode::Normal;
            }
        } else {
            self.show_help = true;
        }
    }

    /// Current spinner glyph (alias the widgets use).
    pub fn spinner_frame(&self) -> &'static str {
        self.spinner()
    }

    /// Friendly wall-clock duration: `340ms`, `1.2s`, `1m05s`,
    /// `2h03m`. Same tiers as the engine's `format_duration_ms`
    /// plus an hour tier (turns and rate-limit waits outgrow minutes).
    pub fn format_friendly_duration(d: std::time::Duration) -> String {
        let ms = d.as_millis().min(u128::from(u64::MAX)) as u64;
        if ms < 1000 {
            format!("{ms}ms")
        } else {
            let s = ms / 1000;
            if s < 60 {
                format!("{:.1}s", ms as f64 / 1000.0)
            } else if s < 3600 {
                format!("{}m{:02}s", s / 60, s % 60)
            } else {
                format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
            }
        }
    }

    /// Friendly `12s`/`1m05s` since the generation started (empty when
    /// idle). Rendered in the status strip next to the spinner phase.
    pub fn elapsed_label(&self) -> String {
        match self.spinner_started {
            Some(s) => Self::format_friendly_duration(s.elapsed()),
            None => String::new(),
        }
    }

    /// Wall-clock the last finished turn took, in friendly form
    /// (`1m05s`). Empty when no turn has completed yet. Shown in the
    /// header as the "what did the *last* turn take" glance, cleared
    /// when the next turn begins. Each reply bubble carries its own
    /// figure too (the `took …` row stamped by `stamp_reply_duration`),
    /// so scrolling back still shows how long an older reply took.
    pub fn last_turn_label(&self) -> String {
        match self.last_turn_duration {
            Some(d) => Self::format_friendly_duration(d),
            None => String::new(),
        }
    }

    /// Whether a finished turn's duration is available for the header.
    pub fn has_last_turn_duration(&self) -> bool {
        self.last_turn_duration.is_some()
    }

    /// Snapshot the running turn's wall-clock as its final duration.
    /// Called on finish / fail / cancel, before `spinner_started` is
    /// cleared. A turn that never started the spinner records nothing.
    pub(super) fn record_turn_duration(&mut self) {
        self.last_turn_duration = self.spinner_started.map(|s| s.elapsed());
    }

    /// Attach the finished turn's wall-clock to the reply bubble this
    /// turn produced, so the chat can render a `took 4.2s` row under the
    /// bubble instead of leaving the figure only in the header.
    ///
    /// Called after the reply is pushed (finish / fail / cancel paths) and
    /// after `record_turn_duration`. `stream_flushed_bubble` is the
    /// per-turn "this turn pushed a reply" flag — it is reset in
    /// `begin_generation` and set wherever an assistant bubble lands — so
    /// a turn that pushed nothing stamps no old bubble with its time.
    pub(super) fn stamp_reply_duration(&mut self) {
        if !self.stream_flushed_bubble {
            return;
        }
        let Some(d) = self.last_turn_duration else {
            return;
        };
        let Some(reply) = self
            .messages
            .iter_mut()
            .rev()
            .find(|m| m.role == MessageRole::Assistant)
        else {
            return;
        };
        reply.metadata.turn_duration_ms = Some(d.as_millis().min(u128::from(u64::MAX)) as u64);
    }

    /// Milliseconds from `begin_generation` to the first non-empty
    /// streamed text chunk of the current turn. `None` before that
    /// chunk lands, and after the turn ends (both `finish_response`
    /// and `fail_generation` clear it).
    ///
    /// Wall-clock measured on the TUI thread, not by the engine.
    /// The two are close enough for a status-bar figure; if the
    /// engine ever wants to make this exact (provider-reported
    /// timing, sub-millisecond precision), it should emit a
    /// dedicated event, not have the TUI measure a proxy.
    /// The current turn's streaming rate in tokens-per-second, if a
    /// rate can be measured. `None` before the second chunk arrives
    /// (a single chunk yields a zero duration, which is a division by
    /// zero, and is also not a meaningful measurement), and after the
    /// turn ends.
    ///
    /// The figure is `(streamed_chars / 4) / seconds-since-first-chunk`
    /// — the same 4-chars-per-token approximation the rest of the TUI
    /// uses. A local model at ~30 tokens/sec shows ~30; a stalled
    /// stream decays toward 0 as the elapsed time grows without a
    /// fresh chunk. The status widget reads it once per frame.
    pub fn tokens_per_sec(&self) -> Option<f32> {
        let first = self.first_chunk_at?;
        let last = self.last_chunk_at?;
        // Require at least two chunks' worth of elapsed time so the
        // first-chunk case does not produce a divide-by-zero or an
        // absurd instantaneous rate.
        let elapsed = last.duration_since(first).as_secs_f32();
        if elapsed < 0.05 {
            return None;
        }
        let tokens = self.streamed_chars_this_turn as f32 / 4.0;
        Some(tokens / elapsed)
    }

    pub fn ttft_ms(&self) -> Option<u128> {
        let started = self.spinner_started?;
        let first = self.first_chunk_at?;
        Some(first.duration_since(started).as_millis())
    }

    /// Consecutive generation failures (offline badge + backoff hints).
    pub fn consecutive_failures(&self) -> usize {
        self.fail_count
    }

    /// Reset the failure streak (after `/retry` or a success).
    pub fn reset_failures(&mut self) {
        self.fail_count = 0;
    }

    /// The taint banner last pushed (see `last_taint_note`). `None`
    /// when the round is clean or no banner has been shown.
    pub fn last_taint_note(&self) -> Option<&str> {
        self.last_taint_note.as_deref()
    }

    /// Set (or clear) the taint-banner guard. Called by the
    /// `ResponseComplete` arm; kept here so the field stays private to
    /// the app module.
    pub fn set_last_taint_note(&mut self, note: Option<String>) {
        self.last_taint_note = note;
    }

    /// Raw last error. Kept for callers that want the unadorned text
    /// (logs, overlays); it is never rendered near the input — the
    /// user-facing error lives in the chat scroll view (see
    /// `fail_generation`).
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// Clear the stored raw error. The transcript keeps its copy.
    pub fn clear_last_error(&mut self) {
        self.last_error = None;
    }

    /// Active theme name (`dark` / `light`).
    pub fn theme_name(&self) -> &str {
        &self.theme.name
    }

    /// Context fill ratio 0.0–1.0+ (rough estimate, see context_label).
    pub fn context_usage(&self) -> f64 {
        self.context_tokens as f64 / self.context_limit.max(1) as f64
    }

    /// Model name for the header (never blank).
    pub fn model_label(&self) -> &str {
        if self.model_name.is_empty() {
            "no model"
        } else {
            &self.model_name
        }
    }

    /// Whether the effective network access is enabled. Drives the
    /// header's `net:on` badge (D3-C5). The TUI sets this from the
    /// engine's `network_access_setting()` after init; the default is
    /// off, matching `LlmConfig::network_access = false`.
    pub fn network_access_enabled(&self) -> bool {
        self.network_access_enabled
    }

    /// Setter used by `TuiLoop::init_engine`.
    /// The effective sandbox label, or empty when no engine has
    /// been initialized yet. Callers should render a badge only
    /// for a non-empty value.
    pub fn sandbox_label(&self) -> &str {
        &self.sandbox_label
    }

    /// Set the sandbox label. Called by `TuiLoop::init_engine`
    /// once the engine is up and the resolver's choice is known.
    pub fn set_sandbox_label(&mut self, label: String) {
        self.sandbox_label = label;
    }

    pub fn set_network_access_enabled(&mut self, enabled: bool) {
        self.network_access_enabled = enabled;
    }

    /// Total input tokens the provider has reported this session.
    pub fn session_input_tokens(&self) -> usize {
        self.session_input_tokens
    }

    /// Total output tokens the provider has reported this session.
    pub fn session_output_tokens(&self) -> usize {
        self.session_output_tokens
    }

    /// USD spent so far this session, per the pricing the endpoints
    /// reported. 0.0 when no endpoint has a `[pricing]` block — the
    /// caller checks `cost_known()` before rendering a `$` figure,
    /// so a genuine 0.0 from a free local model is distinguishable
    /// from "pricing not configured" only by the second flag, not
    /// by this number.
    pub fn session_cost_usd(&self) -> f64 {
        self.session_cost_usd
    }

    /// True once at least one reported call has contributed a cost.
    /// The header renders the `$` figure only when this is true, so
    /// an endpoint without pricing never produces a fake `$0.00`.
    pub fn cost_known(&self) -> bool {
        self.session_cost_usd > 0.0
    }

    /// Install a batch of pending approvals and show the dialog.
    pub fn set_pending_batch(&mut self, batch: PendingApprovalBatch) {
        self.pending_batch = Some(batch);
    }

    /// The pending batch, if any.
    pub fn pending_batch(&self) -> Option<&PendingApprovalBatch> {
        self.pending_batch.as_ref()
    }

    /// Mutable access, used by the key handler to advance through
    /// items without rebuilding the batch.
    pub fn pending_batch_mut(&mut self) -> Option<&mut PendingApprovalBatch> {
        self.pending_batch.as_mut()
    }

    /// The current item of the pending batch, if any. Provided as a
    /// convenience for the widget, which only ever renders one item
    /// at a time; the batch itself is exposed by `pending_batch()`.
    pub fn pending_approval(&self) -> Option<&PendingApproval> {
        self.pending_batch.as_ref().and_then(|b| b.current_item())
    }

    /// Clear the dialog. Called after every item is decided or Esc
    /// aborts the remainder.
    pub fn clear_pending_approval(&mut self) {
        self.pending_batch = None;
    }

    /// True when the approval dialog is up and normal key handling
    /// must be routed to it instead of the input box.
    pub fn is_approving(&self) -> bool {
        self.pending_batch.is_some()
    }

    pub fn set_pending_question(&mut self, q: PendingQuestion) {
        self.pending_question = Some(q);
        self.question_input.clear();
    }

    pub fn pending_question(&self) -> Option<&PendingQuestion> {
        self.pending_question.as_ref()
    }

    pub fn clear_pending_question(&mut self) -> String {
        let text = std::mem::take(&mut self.question_input);
        self.pending_question = None;
        text
    }

    pub fn is_asking(&self) -> bool {
        self.pending_question.is_some()
    }

    pub fn question_input(&self) -> &str {
        &self.question_input
    }

    pub fn question_input_mut(&mut self) -> &mut String {
        &mut self.question_input
    }

    pub fn request_confirm(&mut self, kind: ConfirmKind) {
        self.pending_confirm = Some(kind);
    }

    pub fn pending_confirm(&self) -> Option<ConfirmKind> {
        self.pending_confirm
    }

    pub fn resolve_confirm(&mut self, confirmed: bool) -> Option<ConfirmKind> {
        let kind = self.pending_confirm.take()?;
        if confirmed {
            match kind {
                ConfirmKind::Clear => self.clear_messages(),
                ConfirmKind::Quit => self.should_quit = true,
                // The actual memory+checkpoint clear happens in the
                // main loop, which owns the engine handle. Resolving
                // here just clears the visible chat.
                ConfirmKind::ClearAll => self.clear_messages(),
            }
        }
        Some(kind)
    }

    pub fn is_editing_approval(&self) -> bool {
        self.pending_edit.is_some()
    }

    pub fn edit_buffer(&self) -> Option<&str> {
        self.pending_edit.as_ref().map(|e| e.buffer.as_str())
    }

    pub fn begin_edit_current_approval(&mut self) {
        let Some(batch) = self.pending_batch.as_ref() else {
            return;
        };
        let Some(item) = batch.current_item() else {
            return;
        };
        let buffer =
            serde_json::to_string_pretty(&item.arguments).unwrap_or_else(|_| "{}".to_string());
        self.pending_edit = Some(PendingEdit {
            approval_id: item.id,
            original: item.arguments.clone(),
            buffer,
        });
    }

    pub fn cancel_edit(&mut self) {
        self.pending_edit = None;
    }

    pub fn edit_push_char(&mut self, c: char) {
        if let Some(e) = self.pending_edit.as_mut() {
            e.buffer.push(c);
        }
    }

    pub fn edit_push_newline(&mut self) {
        if let Some(e) = self.pending_edit.as_mut() {
            e.buffer.push('\n');
        }
    }

    pub fn edit_backspace(&mut self) {
        if let Some(e) = self.pending_edit.as_mut() {
            e.buffer.pop();
        }
    }

    /// Parse the buffer; on success, return `(id, arguments)` and
    /// leave the modal closed. On parse failure the modal stays open
    /// and `None` is returned.
    pub fn edit_commit(&mut self) -> Option<(u64, serde_json::Value)> {
        let Some(edit) = self.pending_edit.as_ref() else {
            return None;
        };
        let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&edit.buffer) else {
            return None;
        };
        let id = edit.approval_id;
        // Apply to the pending item so the dialog shows the new args.
        if let Some(batch) = self.pending_batch.as_mut()
            && let Some(item) = batch.items.iter_mut().find(|i| i.id == id)
        {
            item.arguments = parsed.clone();
        }
        self.pending_edit = None;
        Some((id, parsed))
    }

    /// Clear everything that is not the chat itself: the input box,
    /// search state, tool expansion, attachments, dialogs, history
    /// index, and completion state. Returns a count of the fields that
    /// were actually non-empty before the reset, so a caller can tell
    /// the user what was cleared.
    ///
    /// Distinct from `/clear` (which wipes the chat and the engine
    /// transcript) and `/clearall` (which also wipes memory and
    /// checkpoints). This one is the "reset my terminal to a clean
    /// state" — no data is discarded.
    pub fn reset_transient_state(&mut self) -> usize {
        let mut cleared = 0usize;
        if !self.input.is_empty() {
            cleared += 1;
        }
        if !self.question_input.is_empty() {
            cleared += 1;
        }
        if self.search_query.is_some() {
            cleared += 1;
        }
        if !self.expanded_tools.is_empty() {
            cleared += 1;
        }
        if self.pending_batch.is_some() {
            cleared += 1;
        }
        if self.pending_question.is_some() {
            cleared += 1;
        }
        if !self.attached_files.is_empty() {
            cleared += 1;
        }
        if self.last_error.is_some() {
            cleared += 1;
        }

        self.clear_input();
        self.clear_search();
        self.expanded_tools.clear();
        self.pending_batch = None;
        self.pending_question = None;
        self.question_input.clear();
        self.attached_files.clear();
        self.last_error = None;
        self.completion_index = 0;
        self.history_index = None;
        self.draft.clear();
        self.set_input_mode(InputMode::Normal);
        self.mode = AppMode::Normal;

        cleared
    }

    /// Context-aware hint line for the status bar.
    pub fn hint_line(&self) -> String {
        if matches!(self.input_mode, InputMode::Insert) {
            "Enter send · Ctrl+J newline · Up history · Tab complete · Esc done".to_string()
        } else if self.generating {
            "Esc cancel · /steer redirect · ? help".to_string()
        } else {
            // `/ search` used to sit here, but the search key is `f`
            // (SearchPrefix); `/` opens the command slot. Name the
            // actual key so the hint is not a small lie.
            // Kept ≤80 cells: on a standard 80-column terminal the
            // status strip is a single Line and anything past the
            // edge is truncated (which once ate `q quit`). Count
            // before adding segments here.
            "i type · / command · j/k scroll · f search · m mouse · ? help · q quit".to_string()
        }
    }

    /// Last assistant reply text (for `y` / `/copy`).
    pub fn last_assistant_text(&self) -> Option<&str> {
        self.messages
            .iter()
            .rev()
            .find(|m| m.role == MessageRole::Assistant)
            .map(|m| m.content.as_str())
    }

    /// Expand the newest collapsed tool message in place (key `o`).
    /// Returns false when there is nothing expandable.
    pub fn expand_newest_tool(&mut self) -> bool {
        let id = self
            .messages
            .iter()
            .rev()
            .find(|m| m.role == MessageRole::Tool && !self.expanded_tools.contains(&m.id))
            .map(|m| m.id.clone());
        match id {
            Some(id) => {
                self.expanded_tools.insert(id);
                self.scroll_to_bottom();
                true
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::KodApp;
    use std::time::Duration;

    #[test]
    fn friendly_duration_picks_tier_by_magnitude() {
        let f = KodApp::format_friendly_duration;
        assert_eq!(f(Duration::from_millis(0)), "0ms");
        assert_eq!(f(Duration::from_millis(340)), "340ms");
        assert_eq!(f(Duration::from_millis(999)), "999ms");
        assert_eq!(f(Duration::from_millis(1200)), "1.2s");
        assert_eq!(f(Duration::from_secs(45)), "45.0s");
        assert_eq!(f(Duration::from_secs(65)), "1m05s");
        assert_eq!(f(Duration::from_secs(1200)), "20m00s");
        assert_eq!(f(Duration::from_secs(4500)), "1h15m");
    }
}
