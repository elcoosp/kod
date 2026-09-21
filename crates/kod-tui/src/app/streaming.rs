//! Streaming response lifecycle on `KodApp`: the chunk-by-chunk path
//! that a live assistant reply takes from `start_response_stream`
//! through `finish_response`, plus its failure modes
//! (`fail_generation`, `cancel_generation`) and the per-tick spinner
//! state.
//!
//! Extracted from `app/mod.rs` (S9). The methods are declared in a
//! second `impl KodApp` block; Rust resolves them against the same
//! type, so a caller in `mod.rs` writes `app.add_response_chunk(...)`
//! exactly as before.

use super::*;

impl KodApp {
    pub fn current_response(&self) -> &str {
        &self.current_response
    }

    pub fn is_streaming(&self) -> bool {
        self.is_streaming
    }

    pub fn start_response_stream(&mut self) {
        self.is_streaming = true;
        self.current_response.clear();
    }

    pub fn add_response_chunk(&mut self, chunk: &str) {
        if self.is_streaming {
            // The first non-empty text chunk of a turn is the
            // moment the model's answer started arriving. Recorded
            // once; subsequent chunks are what they are. Empty
            // chunks (an SSE keep-alive, a provider that emits a
            // zero-length text fragment) do not count — "the model
            // started answering" is not true when nothing was said.
            if !chunk.is_empty() && self.first_chunk_at.is_none() {
                self.first_chunk_at = Some(Instant::now());
            }
            if !chunk.is_empty() {
                self.last_chunk_at = Some(Instant::now());
                self.streamed_chars_this_turn =
                    self.streamed_chars_this_turn.saturating_add(chunk.len());
            }
            self.current_response.push_str(chunk);
            if self.phase == GenPhase::Connecting {
                self.set_phase(GenPhase::Generating);
            }
        }
    }

    pub fn complete_response(&mut self) {
        if self.is_streaming {
            let response = Self::trim_blank_lines(&self.current_response);
            if !response.is_empty() {
                self.add_message(Message {
                    id: MessageId::new(),
                    role: MessageRole::Assistant,
                    content: response,
                    timestamp: Utc::now(),
                    metadata: MessageMetadata::default(),
                    sequence: 0,
                });
                self.stream_flushed_bubble = true;
            }

            self.is_streaming = false;
            self.current_response.clear();
        }
    }

    /// Mark a prompt as sent: shows the thinking indicator until the
    /// response (or error) arrives. Also arms the streaming accumulator
    /// so `ResponseChunk` events are kept.
    pub fn begin_generation(&mut self) {
        self.generating = true;
        self.spinner_started = Some(Instant::now());
        self.first_chunk_at = None;
        self.turn_started_at = Some(Instant::now());
        self.stream_flushed_bubble = false;
        self.last_error = None;
        // New turn: no real usage seen yet, so the char estimate
        // contributes until (or unless) the provider reports a total.
        self.turn_has_real_usage = false;
        // Reset the streaming-rate state (see `tokens_per_sec`).
        self.last_chunk_at = None;
        self.streamed_chars_this_turn = 0;
        self.set_phase(GenPhase::Connecting);
        self.start_response_stream();
    }

    /// Flush whatever the stream accumulated so far as its own assistant
    /// bubble, keeping the stream open. Called when a tool call starts or
    /// lands so text stays interleaved with tool rows in event order
    /// instead of merging into one giant trailing bubble.
    /// P5.6 — drop whatever the current streaming attempt has
    /// accumulated without pushing it as a bubble. Called when the
    /// engine signals an off-track abort; the next endpoint's
    /// stream then lands in a clean state. Resets the per-turn
    /// char counters so `tokens_per_sec` does not report a phantom
    /// rate from chunks that were thrown away.
    pub fn drop_response_stream(&mut self) {
        self.current_response.clear();
        self.first_chunk_at = None;
        self.last_chunk_at = None;
        self.streamed_chars_this_turn = 0;
        self.set_phase(GenPhase::Connecting);
    }

    pub fn flush_streamed_text(&mut self) {
        let text = Self::trim_blank_lines(&self.current_response);
        self.current_response.clear();
        if !text.is_empty() {
            self.note_usage(text.len());
            self.push_assistant_message(&text);
            self.stream_flushed_bubble = true;
            self.maybe_compact();
        }
    }

    /// Finish a generation. Pushes whatever the stream accumulated, or
    /// `fallback` (the engine's full reply text) when nothing streamed.
    /// When per-round text was already flushed into bubbles (tool runs),
    /// the accumulator is empty *because it was shown* — pushing the
    /// fallback then would reprint the whole reply as one giant trailing
    /// bubble, so it is skipped. Always prefer streamed chunks; fallback
    /// only when nothing was ever flushed.
    pub fn finish_response(&mut self, fallback: &str) {
        let text = if !self.current_response.is_empty() {
            Self::trim_blank_lines(&self.current_response.clone())
        } else if self.stream_flushed_bubble {
            String::new()
        } else {
            Self::trim_blank_lines(fallback)
        };
        if !text.is_empty() {
            self.note_usage(text.len());
            self.push_assistant_message(&text);
            self.stream_flushed_bubble = true;
            self.maybe_compact();
        }

        // Force the chat to show the bottom of the new message — previously
        // push_assistant_message only pinned when already at bottom, so an
        // even-slight scroll-up left the final line clipped and G/End could
        // miscompute the viewport. Chat apps always land at bottom.
        self.scroll_to_bottom();

        self.is_streaming = false;
        self.current_response.clear();
        self.generating = false;
        self.spinner_started = None;
        self.first_chunk_at = None;
        self.set_phase(GenPhase::Idle);
        self.fail_count = 0;
    }

    /// Clear the live "running …" line and settle any entries still
    /// marked Running (error / cancel paths). Without this the indicator
    /// sticks around and later messages land around it unpredictably.
    fn settle_running_tools(&mut self, status: ToolStatus, note: &str) {
        for execution in self.tool_executions.iter_mut() {
            if execution.status == ToolStatus::Running {
                execution.status = status;
                execution.result = Some(note.to_string());
            }
        }
        // Stamp live rows too so cancel/error never leaves a stale
        // "running…" row behind.
        for m in self.messages.iter_mut() {
            if m.role != MessageRole::Tool {
                continue;
            }
            let mut parts = m.content.splitn(2, '\n');
            parts.next();
            if parts.next().unwrap_or("").trim() == Self::LIVE_TOOL_BODY_PLACEHOLDER {
                let header = m.content.lines().next().unwrap_or("").to_string();
                m.content = format!("{header}\n{note}");
            }
        }
        self.current_tool = None;
    }

    /// Record a generation failure: clears the thinking indicator and
    /// surfaces an actionable error as a system message.
    pub fn fail_generation(&mut self, error: &str) {
        // Same rationale as cancel_generation: settle the flag so the
        // next turn does not inherit "real usage seen" from this one.
        self.turn_has_real_usage = false;
        self.is_streaming = false;
        self.current_response.clear();
        self.generating = false;
        self.spinner_started = None;
        self.first_chunk_at = None;
        self.set_phase(GenPhase::Idle);
        self.fail_count += 1;
        self.settle_running_tools(ToolStatus::Failed, error);
        self.last_error = Some(error.to_string());
        self.push_system_message(&Self::friendly_error(error, self.fail_count));
    }

    /// Turn a raw provider/transport error into something the user can act
    /// on. The original text is always kept — advice is appended.
    ///
    /// Ordering matters: "model not found" is often reported by OpenAI-
    /// compatible servers as a 404, and context-length errors sometimes
    /// arrive wrapped in a `400` or `422`. Match the most specific
    /// phrasing first.
    pub fn friendly_error(error: &str, fail_count: usize) -> String {
        let lower = error.to_lowercase();
        let advice = if lower.contains("model not found")
            || lower.contains("model `")
            || lower.contains("unknown model")
            || lower.contains("no such model")
            || lower.contains("model does not exist")
        {
            // The most common first-run mistake: config or `/model <name>`
            // names a model the server has never pulled. The fix is one
            // command, so name it.
            " The model named in the config or `/model` is not on the server.              Pull it first (e.g. `ollama pull codellama:13b`), or run `/model <name>`              with a model the server already has."
        } else if lower.contains("context length")
            || lower.contains("context window")
            || lower.contains("too many tokens")
            || lower.contains("maximum context")
            || lower.contains("exceeds the maximum")
        {
            " The prompt exceeded the model's context window. Run `/compact` to trim              the session, or start a fresh chat with `/clear`."
        } else if (lower.contains("json") && lower.contains("parse"))
            || lower.contains("invalid tool")
            || lower.contains("malformed function")
            || lower.contains("tool_call")
        {
            " The model returned a tool call that could not be parsed. Retrying              usually helps — if it persists, the model may not support tool calling              at all (try a larger or newer model, or a codellama/qwen2.5-coder build)."
        } else if lower.contains("connection refused")
            || lower.contains("connection reset")
            || lower.contains("failed to connect")
            || lower.contains("connection closed")
        {
            " Could not reach the model server — is it running? For Ollama: `ollama serve`, then check `base_url` in the kod config."
        } else if lower.contains("401")
            || lower.contains("unauthorized")
            || lower.contains("api key")
        {
            " Looks like an auth problem — check `api_key` in the kod config."
        } else if lower.contains("404") {
            " Endpoint not found — check `base_url` ends with `/v1` for OpenAI-compatible servers."
        } else if lower.contains("timed out")
            || lower.contains("timeout")
            || lower.contains("deadline")
        {
            " The request timed out — the model may still be loading (first run pulls weights). Wait a minute and `/retry`."
        } else {
            // Any other error carries no situational advice. This also
            // covers "cancelled by user" (which the engine treats as a
            // normal stop, not a failure) and matches the previous
            // behavior of returning an empty advice string for both.
            ""
        };
        let mut out = format!("Error: {error}");
        out.push_str(advice);
        if fail_count >= 2 {
            out.push_str(
                " (offline mode: generation keeps failing — fix the server, then `/retry`)",
            );
        }
        out
    }

    /// Cancel a running prompt (Esc / Ctrl+C / `/cancel`). Keeps whatever
    /// text already streamed as a partial assistant reply, then announces
    /// the cancel so the stop is visible — not a silent stall.
    pub fn cancel_generation(&mut self) {
        // The turn is over; a subsequent prompt must see a fresh
        // turn state even if begin_generation is not called before
        // next note_prompt. begin_generation resets this again.
        self.turn_has_real_usage = false;
        let partial = Self::trim_blank_lines(&self.current_response);
        self.is_streaming = false;
        self.current_response.clear();
        self.generating = false;
        self.spinner_started = None;
        self.first_chunk_at = None;
        self.set_phase(GenPhase::Idle);
        self.settle_running_tools(ToolStatus::Failed, "cancelled");
        if !partial.is_empty() {
            self.note_usage(partial.len());
            self.add_message(Message {
                id: MessageId::new(),
                role: MessageRole::Assistant,
                content: format!("{partial}\n(cancelled — partial answer)"),
                timestamp: Utc::now(),
                metadata: MessageMetadata::default(),
                sequence: 0,
            });
            self.stream_flushed_bubble = true;
        } else {
            self.push_system_message("Cancelled.");
        }
    }

    /// Set the persistent goal (`/goal <text>`). While set, every prompt
    /// runs the goal loop: the agent keeps working turn by turn until it
    /// writes GOAL MET. Shown in the header.
    pub fn set_goal(&mut self, goal: &str) {
        let goal = goal.trim();
        if goal.is_empty() {
            return;
        }
        self.goal = Some(goal.to_string());
    }

    pub fn clear_goal(&mut self) {
        self.goal = None;
    }

    pub fn goal(&self) -> Option<&str> {
        self.goal.as_deref()
    }

    pub fn is_generating(&self) -> bool {
        self.generating
    }

    pub fn set_generating(&mut self, generating: bool) {
        self.generating = generating;
    }

    /// Advance the thinking spinner one frame (called on every tick).
    /// Kept for tick-driven callers; the live frame is time-based (see
    /// `spinner`) so chunk floods can't freeze it.
    pub fn tick(&mut self) {
        self.spinner_frame = self.spinner_frame.wrapping_add(1);
    }

    /// Current spinner frame. While generating, the frame derives from
    /// wall-clock time (≈10 fps), so every render advances it even when
    /// `ResponseChunk` traffic starves `Tick` events.
    pub fn spinner(&self) -> &'static str {
        if self.generating
            && let Some(started) = self.spinner_started
        {
            let step = started.elapsed().as_millis() / 100;
            return SPINNER_FRAMES[(step as usize) % SPINNER_FRAMES.len()];
        }
        SPINNER_FRAMES[self.spinner_frame % SPINNER_FRAMES.len()]
    }
}
