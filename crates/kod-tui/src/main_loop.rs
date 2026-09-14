//! Main TUI loop - coordinates rendering and event handling.

use crate::{
    app::{AppMode, ConfirmKind, InputMode, KodApp},
    event::{Event, EventHandler, KeyCode},
    theme::Theme,
    ui::{
        AgentPanelWidget, ChatWidget, CompletionsWidget, HeaderWidget, HelpWidget, InputWidget,
        StatusWidget,
    },
};
use kod_config::{KodConfig, LlmConfig};
use kod_core::{KodEngine, RouterConfig};
use kod_error::{KodError, Result};
use kod_provider_openai::OpenAICompatProvider;
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
};
use std::io::Stdout;
use std::sync::Arc;
use std::time::Duration;

/// Help text for the `/help` command
const SLASH_HELP: &str = "Commands:\n/help — show this help\n/clear — clear chat (asks confirm)\n/undo — restore last /clear\n/model <name> — switch model\n/skills — list loaded skills\n/goal <text> — set a goal the agent works toward until GOAL MET (/goal clear to stop)\n/steer <instruction> — redirect the running prompt after its current tool call\n/cancel — stop the running prompt (also Esc or Ctrl+C while it runs)\n/compact — compact session history now\n/retry — resend the last prompt\n/search [<text>] — search chat (n/N next/prev, Esc clears)\n/copy — copy last assistant reply to clipboard (also `y`)\n/theme [dark|light] — cycle or set theme\n/tools — toggle tool-output visibility (also `t`)\n/quit — quit kod\n\nWhile a prompt runs, typing + Enter steers it (same as /steer).\nKeys: i insert · j/k scroll · wheel scrolls · q quit · j/k scroll · PgUp/PgDn/Home/End · g/G top/bottom · t toggle tools · o expand · y copy · r retry · u undo · f search · ? help · Esc cancel — hold Option/Shift to select text";

/// Main TUI application loop
pub struct TuiLoop {
    app: KodApp,
    event_handler: EventHandler,
    terminal: Option<Terminal<CrosstermBackend<Stdout>>>,
    engine: Option<Arc<KodEngine>>,
    llm_config: Option<LlmConfig>,
    /// Background generation task. Kept so Esc / Ctrl+C / `/cancel` can
    /// abort it; cleared when the response (or error) lands.
    gen_task: Option<tokio::task::JoinHandle<()>>,
}

impl TuiLoop {
    pub fn new() -> Self {
        Self {
            app: KodApp::new(),
            event_handler: EventHandler::new(Duration::from_millis(100)),
            terminal: None,
            engine: None,
            llm_config: None,
            gen_task: None,
        }
    }

    /// Set up the engine with the OpenAI-compatible provider
    pub async fn init_engine(&mut self, model: Option<String>) -> Result<()> {
        let config = KodConfig::load_default()?;
        let model_name = model.unwrap_or_else(|| config.llm.model.clone());

        // Compute skills_dirs before `config.llm` is moved into
        // self.llm_config below — skills_dirs() borrows &self.config, and
        // the move would make that borrow illegal.
        let skills_dirs = config.skills_dirs()?;

        // The meter + compaction threshold must use the real window from
        // config (e.g. 8k for a small local model), not DEFAULT_CONTEXT_LIMIT.
        self.app.set_context_limit(config.llm.context_window);

        let home = dirs::home_dir()
            .ok_or_else(|| KodError::Config("Could not determine home directory".to_string()))?;
        // KOD_TEST_DB isolates integration tests from a live session's database.
        let db_path = std::env::var("KOD_TEST_DB")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| home.join(".kod").join("data").join("kod.redb"));
        let _ = std::fs::create_dir_all(db_path.parent().unwrap());

        let router_config = RouterConfig::default();
        let engine = KodEngine::new(router_config, db_path)?;

        let provider = OpenAICompatProvider::from_config(&config.llm, Some(&model_name))?;
        engine.set_provider(Arc::new(provider)).await;

        engine.start().await?;
        self.engine = Some(Arc::new(engine));
        self.llm_config = Some(config.llm);
        self.app.set_model_name(&model_name);

        // Load skills from every location KodConfig knows about. The
        // same set is used by `kod skills` and `kod chat`, so the TUI
        // and CLI always agree on the inventory.
        if let Some(engine) = &self.engine {
            match engine.load_skills_from_dirs(&skills_dirs).await {
                Ok(0) => {}
                Ok(n) => tracing::info!("Loaded {} skill file(s)", n),
                Err(e) => tracing::warn!("Could not load skills: {}", e),
            }
            let loaded: Vec<String> = engine.loaded_skill_names().await;
            self.app.set_loaded_skills(loaded);
        }

        // Load model list from provider so /model tab-completion is useful.
        if let Some(engine) = &self.engine {
            let models = engine.list_models().await;
            self.app.set_available_models(models);
        }

        // Restore a saved session, if any. The chat messages come back
        // (what the user sees), and we seed the engine's transcript with
        // the same user/assistant pairs (what the model sees) so the two
        // views agree on the next prompt — without the seed, the model
        // opens the next turn with "this is a fresh conversation" while
        // the screen is full of history.
        let restored = self.app.load_session();
        if restored > 0 {
            if let Some(engine) = &self.engine {
                for m in self.app.messages() {
                    match m.role {
                        kod_types::MessageRole::User => {
                            engine.seed_turn(true, &m.content).await;
                        }
                        kod_types::MessageRole::Assistant => {
                            engine.seed_turn(false, &m.content).await;
                        }
                        // System / Tool / Agent messages are display-only:
                        // they never reached the model as turns, so they
                        // should not enter the model's transcript now.
                        _ => {}
                    }
                }
            }
            self.app.push_system_message(&format!(
                "Restored {} message(s) · model {} · {} skill(s)",
                restored,
                model_name,
                self.app.loaded_skills().len()
            ));
        } else {
            // Welcome line so an empty screen never looks dead.
            self.app.push_system_message(&format!(
                "Connected · model {} · {} skill(s) · type /help for commands",
                model_name,
                self.app.loaded_skills().len()
            ));
        }

        Ok(())
    }

    pub fn is_initialized(&self) -> bool {
        true
    }

    pub fn app(&self) -> &KodApp {
        &self.app
    }

    pub fn app_mut(&mut self) -> &mut KodApp {
        &mut self.app
    }

    /// Wait for the next event from the keyboard, background tasks, or tick.
    /// Exposed so the loop (and tests) can drive `handle_event` manually.
    pub async fn next_event(&self) -> Event {
        self.event_handler.next_event().await
    }

    /// Initialize terminal
    pub async fn init_terminal(&mut self) -> Result<()> {
        crossterm::terminal::enable_raw_mode()
            .map_err(|e| KodError::Internal(format!("Failed to enable raw mode: {}", e)))?;

        // Mouse capture always on: wheel scrolls reliably and text
        // selection still works by holding Option (macOS) or Shift while
        // dragging. No `/mouse` toggle — one mode, no surprise.
        crossterm::execute!(
            std::io::stdout(),
            crossterm::terminal::EnterAlternateScreen,
            crossterm::event::EnableMouseCapture,
        )
        .map_err(|e| KodError::Internal(format!("Failed to enter alternate screen: {e}")))?;

        let backend = CrosstermBackend::new(std::io::stdout());
        let terminal = Terminal::new(backend)
            .map_err(|e| KodError::Internal(format!("Failed to create terminal: {}", e)))?;

        self.terminal = Some(terminal);

        self.event_handler.start_input_loop().await;

        Ok(())
    }

    /// Restore terminal
    pub async fn restore_terminal(&mut self) -> Result<()> {
        self.event_handler.stop();

        if let Some(terminal) = &mut self.terminal {
            terminal
                .show_cursor()
                .map_err(|e| KodError::Internal(format!("Failed to show cursor: {}", e)))?;
        }

        crossterm::execute!(
            std::io::stdout(),
            crossterm::event::DisableMouseCapture,
            crossterm::terminal::LeaveAlternateScreen,
        )
        .map_err(|e| KodError::Internal(format!("Failed to leave alternate screen: {e}")))?;

        crossterm::terminal::disable_raw_mode()
            .map_err(|e| KodError::Internal(format!("Failed to disable raw mode: {}", e)))?;

        self.terminal = None;

        Ok(())
    }

    /// Run the main loop
    pub async fn run(&mut self, model: Option<String>) -> Result<()> {
        // Install a panic hook that restores the terminal so a single
        // unexpected error doesn't strand the user in a broken screen.
        let default_hook = std::panic::take_hook();
        let default_hook = std::sync::Arc::new(default_hook);
        std::panic::set_hook({
            let default_hook = default_hook.clone();
            Box::new(move |info| {
                // Best-effort terminal restore so the user isn't left
                // staring at a frozen raw-mode screen.
                let _ = crossterm::execute!(
                    std::io::stdout(),
                    crossterm::terminal::LeaveAlternateScreen,
                    crossterm::event::DisableMouseCapture,
                );
                let _ = crossterm::terminal::disable_raw_mode();
                (*default_hook)(info);
            })
        });

        self.init_engine(model).await?;
        self.init_terminal().await?;
        let result = self.main_loop().await;

        // Persist the chat for the next session. Save before restoring
        // the terminal so a crossterm error cannot lose the chat; the
        // save itself is best-effort (see KodApp::save_session).
        self.app.save_session();

        // Restore the original hook before tearing down.
        let default_hook = std::sync::Arc::try_unwrap(default_hook)
            .unwrap_or_else(|_| std::panic::take_hook());
        std::panic::set_hook(default_hook);

        let _ = self.restore_terminal().await;
        result
    }

    /// Internal main loop
    async fn main_loop(&mut self) -> Result<()> {
        while !self.app.should_quit() {
            self.render().await?;

            let event = self.event_handler.next_event().await;
            self.handle_event(event).await?;
        }

        Ok(())
    }

    /// Render the UI: header / chat / completions / status / input(bottom)
    async fn render(&mut self) -> Result<()> {
        if let Some(terminal) = &mut self.terminal {
            terminal
                .draw(|f| {
                    let size = f.area();

                    let popup_height: u16 = if self.app.show_completions() {
                        CompletionsWidget::height(&self.app)
                    } else {
                        0
                    };
                    // The input box grows with multiline content (clamped).
                    let input_height: u16 = self.app.input_height_rows();

                    let rows = if popup_height > 0 {
                        vec![
                            Constraint::Length(1),
                            Constraint::Min(1),
                            Constraint::Length(popup_height),
                            Constraint::Length(1),
                            Constraint::Length(input_height),
                        ]
                    } else {
                        vec![
                            Constraint::Length(1),
                            Constraint::Min(1),
                            Constraint::Length(1),
                            Constraint::Length(input_height),
                        ]
                    };
                    let chunks = Layout::default()
                        .direction(Direction::Vertical)
                        .constraints(rows)
                        .split(size);

                    let (chat_idx, input_idx, popup_idx) = if popup_height > 0 {
                        (1usize, 4usize, Some(2usize))
                    } else {
                        (1usize, 3usize, None)
                    };
                    let status_idx = input_idx - 1;

                    HeaderWidget::new().render(&self.app, chunks[0], f.buffer_mut());

                    if *self.app.mode() == AppMode::AgentPanel {
                        let cols = Layout::default()
                            .direction(Direction::Horizontal)
                            .constraints([Constraint::Percentage(70), Constraint::Percentage(30)])
                            .split(chunks[chat_idx]);
                        ChatWidget::new().render(&self.app, cols[0], f.buffer_mut());
                        AgentPanelWidget::new().render(&self.app, cols[1], f.buffer_mut());
                    } else {
                        ChatWidget::new().render(&self.app, chunks[chat_idx], f.buffer_mut());
                    }

                    if let Some(popup) = popup_idx {
                        CompletionsWidget::new().render(&self.app, chunks[popup], f.buffer_mut());
                    }

                    StatusWidget::new().render(&self.app, chunks[status_idx], f.buffer_mut());
                    InputWidget::new().render(&self.app, chunks[input_idx], f.buffer_mut());

                    if self.app.show_help() {
                        HelpWidget::new().render(&self.app, size, f.buffer_mut());
                    }
                })
                .map_err(|e| KodError::Internal(format!("Failed to draw: {}", e)))?;
        }

        Ok(())
    }

    /// Handle a single event
    pub async fn handle_event(&mut self, event: Event) -> Result<()> {
        match event {
            Event::Key(key_code) => self.handle_key(key_code).await?,
            Event::UserInput(input) => {
                self.app.set_input(input);
                self.dispatch_prompt().await?;
            }
            Event::Quit => {
                self.app.quit();
            }
            Event::Tick => {
                self.app.tick();
            }
            Event::Resize(w, h) => {
                tracing::debug!("Terminal resized to {}x{}", w, h);
            }
            Event::ResponseChunk(chunk) => {
                self.app.add_response_chunk(&chunk);
            }
            Event::ResponseComplete(text) => {
                self.gen_task = None;
                self.app.finish_response(&text);
            }
            Event::TokenUsage(total) => {
                self.app.note_real_usage(total);
            }
            Event::ToolStarted(tool_name) => {
                // Flush text streamed so far as its own bubble first: the
                // reply before the call belongs above the tool row, the
                // reply after it below — never one giant bubble.
                self.app.flush_streamed_text();
                self.app.start_tool_execution(&tool_name);
            }
            Event::ToolProgress(display) => {
                self.app.update_tool_status(&display);
            }
            Event::Thinking => {
                self.app.begin_thinking();
            }
            Event::Cancelled => {
                self.gen_task = None;
                self.app.cancel_generation();
            }
            Event::ToolCompleted(tool_name, result) => {
                // Tool row first, then whatever streamed during the call:
                // flushing before would drop post-tool text above the row.
                self.app.complete_tool_execution(&tool_name, &result);
                self.app.flush_streamed_text();
                // Failures must be unmissable even if the tool row is collapsed
                // or `t` hid tools — push a red system line as backup.
                if result.trim_start().starts_with("Error:") {
                    self.app.push_system_message(&format!(
                        "Tool `{}` failed: {}",
                        tool_name,
                        result.trim()
                    ));
                }
            }
            Event::ToolCompletedWithDuration(tool_name, result, duration_ms) => {
                // Live done-marker: same row fill, stamped with wall time.
                // The task-end `ToolCompleted` fallback for this call (if it
                // arrives) is idempotent and keeps this timed row.
                self.app.complete_tool_execution_with_duration(
                    &tool_name,
                    &result,
                    Some(duration_ms),
                );
                self.app.flush_streamed_text();
                if result.trim_start().starts_with("Error:") {
                    self.app.push_system_message(&format!(
                        "Tool `{}` failed: {}",
                        tool_name,
                        result.trim()
                    ));
                }
            }
            Event::AgentMessage(agent_name, message) => {
                self.app.add_message(crate::app::Message {
                    id: kod_types::MessageId::new(),
                    role: kod_types::MessageRole::Agent(kod_types::AgentId::new()),
                    content: format!("[{}] {}", agent_name, message),
                    timestamp: chrono::Utc::now(),
                    metadata: Default::default(),
                    sequence: 0,
                });
            }
            Event::Error(error) => {
                self.gen_task = None;
                self.app.fail_generation(&error);
            }
            _ => {}
        }

        Ok(())
    }

    /// Submit whatever is in the input box: slash command or LLM prompt.
    async fn dispatch_prompt(&mut self) -> Result<()> {
        let input = self.app.input().to_string();
        if input.trim().is_empty() {
            return Ok(());
        }
        self.app.reset_completion();

        if input.trim_start().starts_with('/') {
            self.app.submit_input();
            self.handle_command(&input).await?;
            return Ok(());
        }

        // While a prompt is running, plain text is not a new prompt — it
        // steers the running one (same as `/steer`).
        if self.app.is_generating() {
            self.app.submit_input();
            if let Some(engine) = self.engine.clone() {
                engine.steer(&input).await;
            }
            self.app.push_system_message(&format!(
                "Steered — applies after the current tool call: {input}"
            ));
            return Ok(());
        }

        self.app.submit_input();

        // Without an engine (e.g. in tests) the message is recorded and
        // nothing else happens.
        let Some(engine) = self.engine.clone() else {
            return Ok(());
        };

        // Track session context before the prompt leaves the TUI.
        self.app.note_prompt(&input);

        // Show the thinking indicator until the response lands. This also
        // arms the streaming accumulator so ResponseChunk events are kept.
        self.app.begin_generation();

        let event_tx = self.event_handler.sender();
        // A previous cancel must not leak into the new prompt.
        if let Some(engine) = &self.engine {
            engine.clear_cancel();
        }
        // With an active `/goal`, the prompt runs the goal loop instead of
        // a single pass (moved into the task: `goal` must be owned).
        let goal = self.app.goal().map(|g| g.to_string());
        let handle = tokio::spawn(async move {
            // Stream the answer: text chunks render live, tool-start
            // markers raise the "running …" indicator, and tool-args
            // markers refresh it with the actual command/file excerpt.
            let (chunk_tx, mut chunk_rx) = tokio::sync::mpsc::channel::<String>(64);
            let event_tx_chunks = event_tx.clone();
            let pump = tokio::spawn(async move {
                while let Some(chunk) = chunk_rx.recv().await {
                    if let Some(tool) = kod_core::engine::parse_tool_start(&chunk) {
                        let _ = event_tx_chunks
                            .send(Event::ToolStarted(tool.to_string()))
                            .await;
                    } else if let Some(progress) = kod_core::engine::parse_tool_args(&chunk) {
                        let _ = event_tx_chunks
                            .send(Event::ToolProgress(progress.to_string()))
                            .await;
                    } else if let Some((header, summary, duration)) =
                        kod_core::engine::parse_tool_done(&chunk)
                    {
                        let _ = event_tx_chunks
                            .send(Event::ToolCompletedWithDuration(
                                header.to_string(),
                                summary.to_string(),
                                duration,
                            ))
                            .await;
                    } else if kod_core::engine::is_thinking_marker(&chunk) {
                        let _ = event_tx_chunks.send(Event::Thinking).await;
                    } else {
                        let _ = event_tx_chunks.send(Event::ResponseChunk(chunk)).await;
                    }
                }
            });
            let result = match goal {
                Some(g) => engine.process_goal_streaming(&input, &g, &chunk_tx).await,
                None => engine.process_streaming(&input, &chunk_tx).await,
            };
            drop(chunk_tx);
            let _ = pump.await;
            match result {
                Ok(response) => {
                    // Pair each result with its own call (names/args match
                    // 1:1 with results in engine order).
                    let calls: Vec<_> = response.tool_calls.iter().collect();
                    for (i, result) in response.tool_results.iter().enumerate() {
                        let call = calls.get(i).copied();
                        let name = call.map(|c| c.tool_name.as_str()).unwrap_or("tool");
                        // Task-end fallback: the live done-marker normally
                        // completed each row already (with its duration) and
                        // this rewrite is idempotent there. Shared with the
                        // engine so headers/summaries match the live ones.
                        let header = match call {
                            Some(c) => {
                                kod_core::engine::format_tool_header(&c.tool_name, &c.arguments)
                            }
                            None => format!("[{}]", name),
                        };
                        let summary = kod_core::engine::summarize_tool_result(name, result);
                        let _ = event_tx.send(Event::ToolCompleted(header, summary)).await;
                    }
                    if !response.skills_used.is_empty() {
                        let _ = event_tx
                            .send(Event::AgentMessage(
                                "skills".to_string(),
                                format!("used: {}", response.skills_used.join(", ")),
                            ))
                            .await;
                    }
                    let text = response.text.unwrap_or_default();
                    if let Some(usage) = response.usage {
                        let total = if usage.total_tokens > 0 {
                            usage.total_tokens
                        } else {
                            usage.prompt_tokens + usage.completion_tokens
                        };
                        let _ = event_tx.send(Event::TokenUsage(total)).await;
                    }
                    let _ = event_tx.send(Event::ResponseComplete(text)).await;
                }
                Err(e) => {
                    let msg = e.to_string();
                    if msg.contains("cancelled by user") {
                        let _ = event_tx.send(Event::Cancelled).await;
                    } else {
                        let _ = event_tx.send(Event::Error(msg)).await;
                    }
                }
            }
        });
        self.gen_task = Some(handle);

        Ok(())
    }

    /// Stop the running prompt: abort the background task, flag the
    /// engine, and settle the UI. No-op when nothing is running.
    /// Esc / Ctrl+C / `/cancel` all land here.
    pub fn cancel_generation(&mut self) {
        if !self.app.is_generating() {
            return;
        }
        if let Some(handle) = self.gen_task.take() {
            handle.abort();
        }
        if let Some(engine) = &self.engine {
            engine.request_cancel();
        }
        self.app.cancel_generation();
    }

    /// Re-send the last prompt (`/retry` or `r` key).
    async fn retry_generation(&mut self) -> Result<()> {
        let prompt = match self.app.last_prompt() {
            Some(p) if !p.is_empty() => p.to_string(),
            _ => {
                self.app
                    .push_system_message("Nothing to retry — no previous prompt.");
                return Ok(());
            }
        };
        // Set input to the last prompt, then dispatch as a normal submit.
        self.app.set_input(prompt.clone());
        self.app.set_last_prompt(&prompt);
        // Box::pin to break the dispatch → handle_command → retry → dispatch
        // recursive future cycle (Rust requires indirection for recursive
        // async fns).
        Box::pin(self.dispatch_prompt()).await
    }

    /// Execute a `/` command (input already recorded as a user message).
    async fn handle_command(&mut self, input: &str) -> Result<()> {
        let mut parts = input.split_whitespace();
        let cmd = parts.next().unwrap_or("");
        match cmd {
            "/help" => {
                self.app.push_system_message(SLASH_HELP);
            }
            "/clear" => {
                if self.app.is_generating() {
                    self.app.push_system_message(
                        "Wait for the current prompt to finish before clearing.",
                    );
                } else {
                    self.app.request_confirm(ConfirmKind::Clear);
                }
            }
            "/quit" => {
                if self.app.is_generating() {
                    self.app.request_confirm(ConfirmKind::Quit);
                } else {
                    self.app.quit();
                }
            }
            "/model" => match parts.next() {
                Some(name) => self.switch_model(name).await?,
                None => {
                    self.app.push_system_message("Usage: /model <name>");
                }
            },
            "/skills" => {
                let details: Vec<(String, String)> = if let Some(engine) = &self.engine {
                    engine.loaded_skill_details().await
                } else {
                    self.app
                        .loaded_skills()
                        .iter()
                        .map(|n| (n.clone(), String::new()))
                        .collect()
                };
                if details.is_empty() {
                    self.app.push_system_message(
                        "No skills loaded. Put skills in ~/.agents/skills (global) or .agents/skills (project), then restart kod.",
                    );
                } else {
                    let lines: Vec<String> = details
                        .iter()
                        .map(|(n, d)| {
                            let d = d.trim();
                            if d.is_empty() {
                                format!("- {n}")
                            } else {
                                let short = if d.len() > 100 {
                                    format!("{}…", &d[..100])
                                } else {
                                    d.to_string()
                                };
                                format!("- {n} — {short}")
                            }
                        })
                        .collect();
                    self.app.push_system_message(&format!(
                        "Skills ({}):\n{}",
                        details.len(),
                        lines.join("\n")
                    ));
                }
            }
            "/cancel" => {
                if self.app.is_generating() {
                    self.cancel_generation();
                } else {
                    self.app.push_system_message("Nothing is running.");
                }
            }
            "/steer" => {
                let note: String = parts.collect::<Vec<_>>().join(" ");
                if note.trim().is_empty() {
                    self.app.push_system_message("Usage: /steer <instruction>");
                } else if self.app.is_generating() {
                    if let Some(engine) = &self.engine {
                        engine.steer(&note).await;
                    }
                    self.app.push_system_message(&format!(
                        "Steered — applies after the current tool call: {note}"
                    ));
                } else {
                    self.app.push_system_message(
                        "Nothing is running — /steer only redirects a live prompt. Send a prompt first.",
                    );
                }
            }
            "/goal" => {
                let rest: String = parts.collect::<Vec<_>>().join(" ");
                let rest = rest.trim();
                if rest.eq_ignore_ascii_case("clear") || rest.eq_ignore_ascii_case("off") {
                    self.app.clear_goal();
                    self.app
                        .push_system_message("Goal cleared — prompts run once again.");
                } else if rest.is_empty() {
                    match self.app.goal() {
                        Some(g) => self.app.push_system_message(&format!(
                            "Active goal: {g}\nPrompts keep working until GOAL MET. /goal clear stops it."
                        )),
                        None => self.app.push_system_message(
                            "Usage: /goal <text> — the agent keeps working turn by turn until it writes GOAL MET. /goal clear stops it.",
                        ),
                    }
                } else {
                    self.app.set_goal(rest);
                    self.app.push_system_message(&format!(
                        "Goal set: {rest}\nEvery prompt now works turn by turn until GOAL MET. Esc cancels; /steer redirects; /goal clear stops."
                    ));
                    // Start working immediately: the goal itself becomes
                    // the first prompt. Call dispatch_prompt directly
                    // instead of pushing a synthetic Enter into the event
                    // queue — the previous approach relied on Enter's
                    // Insert-mode binding and could misfire if the user
                    // changed keybindings or was mid-typing.
                    //
                    // Box::pin breaks the dispatch → handle_command →
                    // (goal arm) → dispatch recursion the same way
                    // retry_generation does.
                    self.app.set_input(rest.to_string());
                    Box::pin(self.dispatch_prompt()).await?;
                }
            }
            "/compact" => {
                self.app.compact_now();
                if let Some(engine) = &self.engine {
                    engine.compact_history(20).await;
                }
            }
            "/retry" => {
                self.retry_generation().await?;
            }
            "/search" => {
                let rest: String = parts.collect::<Vec<_>>().join(" ");
                let rest = rest.trim();
                if rest.is_empty() {
                    self.app.begin_search();
                } else {
                    let n = self.app.set_search(rest);
                    self.app.push_system_message(&format!(
                        "Search: \"{rest}\" — {n} match(es). n/N next/prev, Esc clears."
                    ));
                }
            }
            "/copy" => {
                if self.app.copy_last_to_clipboard() {
                    self.app
                        .push_system_message("Copied last assistant reply to clipboard.");
                } else {
                    self.app
                        .push_system_message("Nothing to copy — no assistant reply yet.");
                }
            }
            "/theme" => match parts.next() {
                Some(name) => {
                    let next = Theme::from_name(name);
                    let prev = self.app.theme_name().to_string();
                    self.app.set_theme(next);
                    self.app
                        .push_system_message(&format!("Theme {prev} → {}", name));
                }
                None => {
                    let cur = self.app.theme_name().to_string();
                    let next = self.app.cycle_theme();
                    self.app
                        .push_system_message(&format!("Theme {cur} → {next}"));
                }
            },
            "/tools" => {
                let on = self.app.toggle_show_tools();
                self.app.push_system_message(if on {
                    "Tool outputs shown."
                } else {
                    "Tool outputs hidden."
                });
            }
            "/undo" => {
                if self.app.undo_clear() {
                    self.app
                        .push_system_message("Restored last cleared messages.");
                } else {
                    self.app.push_system_message("Nothing to undo.");
                }
            }
            _ => {
                self.app
                    .push_system_message(&format!("Unknown command: {} — try /help", cmd));
            }
        }
        Ok(())
    }

    /// Swap the engine's provider to another model on the same endpoint.
    async fn switch_model(&mut self, name: &str) -> Result<()> {
        let (Some(engine), Some(config)) = (self.engine.clone(), self.llm_config.clone()) else {
            self.app.push_system_message("Engine not initialized");
            return Ok(());
        };
        match OpenAICompatProvider::from_config(&config, Some(name)) {
            Ok(provider) => {
                engine.set_provider(Arc::new(provider)).await;
                self.app.set_model_name(name);
                self.app
                    .push_system_message(&format!("Switched model to {}", name));
            }
            Err(e) => {
                self.app
                    .push_system_message(&format!("Could not switch model: {}", e));
            }
        }
        Ok(())
    }

    /// Handle key events
    async fn handle_key(&mut self, key: KeyCode) -> Result<()> {
        // Intercept yes/no when a destructive action is pending (q, /clear).
        if self.app.pending_confirm().is_some() {
            match key {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    let kind = self.app.resolve_confirm(true);
                    if kind == Some(ConfirmKind::Clear)
                        && let Some(engine) = &self.engine
                    {
                        engine.clear_history().await;
                    }
                    return Ok(());
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Escape | KeyCode::CtrlC => {
                    self.app.resolve_confirm(false);
                    return Ok(());
                }
                _ => return Ok(()),
            }
        }
        // Esc clears an active search before anything else (normal or insert)
        if self.app.is_searching() && key == KeyCode::Escape {
            self.app.clear_search();
            self.app.push_system_message("Search cleared.");
            return Ok(());
        }
        match self.app.input_mode() {
            InputMode::Normal => self.handle_normal_mode_key(key).await,
            InputMode::Insert => self.handle_insert_mode_key(key).await,
        }
    }

    /// Handle keys in normal mode
    async fn handle_normal_mode_key(&mut self, key: KeyCode) -> Result<()> {
        // Touch keybindings module so it stays wired (configurable bindings)
        let _ = crate::keybindings::default_bindings();
        match key {
            KeyCode::Char('i') | KeyCode::Char('I') => {
                self.app.set_input_mode(InputMode::Insert);
            }
            KeyCode::Char('q') => {
                if self.app.is_generating() {
                    self.app.request_confirm(ConfirmKind::Quit);
                } else {
                    self.app.quit();
                }
            }
            KeyCode::Char('j') => self.app.scroll_down(1),
            KeyCode::Char('k') => self.app.scroll_up(1),
            KeyCode::Char('g') => self.app.scroll_to_top(),
            KeyCode::Char('G') => self.app.scroll_to_bottom(),
            KeyCode::Char('e') => {
                self.app.edit_last_message();
                self.app.set_input_mode(InputMode::Insert);
            }
            KeyCode::Char('u') => {
                if self.app.undo_clear() {
                    self.app
                        .push_system_message("Restored last cleared messages.");
                } else {
                    self.app.push_system_message("Nothing to undo.");
                }
            }
            KeyCode::Char('t') => {
                let on = self.app.toggle_show_tools();
                self.app.push_system_message(if on {
                    "Tool outputs shown."
                } else {
                    "Tool outputs hidden."
                });
            }
            KeyCode::Char('f') => {
                self.app.set_input("/search ".to_string());
                self.app.set_input_mode(InputMode::Insert);
            }
            KeyCode::Char('y') => {
                if self.app.copy_last_to_clipboard() {
                    self.app
                        .push_system_message("Copied last assistant reply to clipboard.");
                } else {
                    self.app
                        .push_system_message("Nothing to copy — no assistant reply yet.");
                }
            }
            KeyCode::Char('r') => {
                self.retry_generation().await?;
            }
            KeyCode::Char('o') => {
                if self.app.expand_newest_tool() {
                    self.app.push_system_message("Expanded newest tool output.");
                }
            }
            KeyCode::Char('n') => {
                if self.app.is_searching()
                    && let Some((pos, total)) = self.app.search_next()
                {
                    self.app
                        .push_system_message(&format!("Search {pos}/{total}"));
                }
            }
            KeyCode::Char('N') => {
                if self.app.is_searching()
                    && let Some((pos, total)) = self.app.search_prev()
                {
                    self.app
                        .push_system_message(&format!("Search {pos}/{total}"));
                }
            }
            KeyCode::Escape => {
                // Do NOT quit unconditionally — only back out of live states.
                if self.app.is_generating() {
                    self.cancel_generation();
                } else if self.app.show_help() {
                    self.app.toggle_help();
                }
                // is_searching already handled in handle_key above
            }
            KeyCode::CtrlC => {
                self.cancel_generation();
            }
            KeyCode::Tab => match self.app.mode() {
                AppMode::Normal => self.app.set_mode(AppMode::AgentPanel),
                AppMode::AgentPanel => self.app.set_mode(AppMode::Normal),
                _ => self.app.set_mode(AppMode::Normal),
            },
            KeyCode::Char('h') | KeyCode::Char('?') => {
                self.app.toggle_help();
            }
            KeyCode::F(1) => {
                self.app.toggle_help();
            }
            KeyCode::Char('a') => {
                self.app.set_mode(AppMode::AgentPanel);
            }
            KeyCode::Up => {
                self.app.scroll_up(1);
            }
            KeyCode::Down => {
                self.app.scroll_down(1);
            }
            KeyCode::PageUp => {
                self.app.scroll_up(10);
            }
            KeyCode::PageDown => {
                self.app.scroll_down(10);
            }
            KeyCode::Home => {
                self.app.scroll_to_top();
            }
            KeyCode::End => {
                self.app.scroll_to_bottom();
            }
            _ => {}
        }

        Ok(())
    }

    /// Handle keys in insert mode
    async fn handle_insert_mode_key(&mut self, key: KeyCode) -> Result<()> {
        // While the completion popup is open, navigation keys belong to it.
        // Tab picks the highlighted entry; Enter accepts it AND submits,
        // so a typed `/help` + Enter runs instead of going nowhere.
        if self.app.show_completions() {
            match key {
                KeyCode::Tab => {
                    self.app.accept_completion();
                    return Ok(());
                }
                KeyCode::Enter => {
                    self.app.accept_completion();
                }
                KeyCode::Up => {
                    self.app.completion_prev();
                    return Ok(());
                }
                KeyCode::Down => {
                    self.app.completion_next();
                    return Ok(());
                }
                KeyCode::Escape => {
                    self.app.reset_completion();
                    self.app.set_input(String::new());
                    return Ok(());
                }
                _ => {}
            }
        }

        match key {
            KeyCode::Enter => {
                self.dispatch_prompt().await?;
            }
            // Newline inside the input box (Ctrl+J / Shift+Enter).
            // Enter still sends — multiline never traps the user.
            KeyCode::CtrlJ | KeyCode::ShiftEnter => {
                self.app.insert_newline();
            }
            KeyCode::Tab => {
                // No popup (e.g. mid-word slash): try to complete anyway.
                self.app.accept_completion();
            }
            KeyCode::Escape => {
                // Esc in insert ALWAYS just drops to normal — never cancels.
                // Cancelling from insert would make "Esc to leave the box"
                // kill the turn, which is terrible DX. Cancel lives on
                // Ctrl+C (both modes) or Esc in normal mode.
                self.app.reset_completion();
                self.app.set_input_mode(InputMode::Normal);
            }
            KeyCode::CtrlC => {
                self.cancel_generation();
            }
            KeyCode::Backspace => {
                self.app.backspace();
                self.app.reset_completion();
            }
            KeyCode::Delete => {
                if self.app.cursor_position() < self.app.input().len() {
                    let pos = self.app.cursor_position();
                    self.app.set_input({
                        let mut s = self.app.input().to_string();
                        if let Some((idx, ch)) = s[pos..].char_indices().next() {
                            s.drain(pos..idx + ch.len_utf8());
                        }
                        s
                    });
                }
            }
            KeyCode::CtrlU => {
                self.app.delete_to_line_start();
            }
            KeyCode::CtrlW => {
                self.app.delete_word_before();
            }
            KeyCode::CtrlK => {
                self.app.cut_to_end();
            }
            KeyCode::Left => {
                self.app.move_cursor_left();
            }
            KeyCode::Right => {
                self.app.move_cursor_right();
            }
            KeyCode::CtrlLeft => {
                self.app.move_cursor_word_left();
            }
            KeyCode::CtrlRight => {
                self.app.move_cursor_word_right();
            }
            KeyCode::Up => {
                self.app.history_previous();
            }
            KeyCode::Down => {
                self.app.history_next();
            }
            // Conversation scroll must work while typing, not just in
            // normal mode — history keeps Up/Down, scrolling gets PgUp/PgDn.
            KeyCode::PageUp => {
                self.app.scroll_up(10);
            }
            KeyCode::PageDown => {
                self.app.scroll_down(10);
            }
            KeyCode::Home => {
                self.app.scroll_to_top();
            }
            KeyCode::End => {
                self.app.scroll_to_bottom();
            }
            // In insert mode ALL printable characters go to the input.
            // The movement/command keys (j/k/g/G/y/r/t/o/u/f) are Normal-mode
            // only so they don't steal letters from your prompt.
            KeyCode::Char(c) => {
                self.app.add_char(c);
                self.app.reset_completion();
            }
            _ => {}
        }

        Ok(())
    }
}

impl Default for TuiLoop {
    fn default() -> Self {
        Self::new()
    }
}

// Tool header/summary rendering lives in `kod_core::engine`
// (`format_tool_header`, `summarize_tool_result`): the engine needs it
// for the live done-markers and the TUI for the task-end fallback, so
// both share one implementation instead of drifting apart.

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_tui_lifecycle() {
        let mut tui = TuiLoop::new();

        tui.handle_event(Event::Key(KeyCode::Char('i')))
            .await
            .unwrap();
        assert_eq!(tui.app().input_mode(), &InputMode::Insert);

        tui.handle_event(Event::Key(KeyCode::Escape)).await.unwrap();
        assert_eq!(tui.app().input_mode(), &InputMode::Normal);
    }

    #[tokio::test]
    async fn test_ctrlc_cancels_running_prompt() {
        let mut tui = TuiLoop::new();
        tui.app_mut().begin_generation();
        assert!(tui.app().is_generating());
        tui.handle_event(Event::Key(KeyCode::CtrlC)).await.unwrap();
        assert!(!tui.app().is_generating());
        let last = tui.app().messages().last().unwrap();
        assert!(last.content.contains("Cancelled"), "got: {}", last.content);
    }

    #[tokio::test]
    async fn test_escape_in_insert_drops_to_normal_without_cancelling() {
        let mut tui = TuiLoop::new();
        tui.app_mut().set_input_mode(InputMode::Insert);
        tui.app_mut().begin_generation();
        tui.handle_event(Event::Key(KeyCode::Escape)).await.unwrap();
        assert!(
            tui.app().is_generating(),
            "Esc in insert must not cancel generation"
        );
        assert_eq!(tui.app().input_mode(), &InputMode::Normal);
    }

    #[tokio::test]
    async fn test_escape_in_normal_cancels_running_prompt() {
        let mut tui = TuiLoop::new();
        tui.app_mut().set_input_mode(InputMode::Normal);
        tui.app_mut().begin_generation();
        tui.handle_event(Event::Key(KeyCode::Escape)).await.unwrap();
        assert!(!tui.app().is_generating());
        assert_eq!(tui.app().input_mode(), &InputMode::Normal);
    }

    #[tokio::test]
    async fn test_enter_while_running_steers_instead_of_prompting() {
        let mut tui = TuiLoop::new();
        tui.app_mut().set_input_mode(InputMode::Insert);
        tui.app_mut().set_input("go left".to_string());
        tui.app_mut().begin_generation();
        tui.handle_event(Event::Key(KeyCode::Enter)).await.unwrap();
        // Steering must not end the run.
        assert!(tui.app().is_generating());
        let last = tui.app().messages().last().unwrap();
        assert!(last.content.contains("Steered"), "got: {}", last.content);
    }

    #[tokio::test]
    async fn test_tool_progress_refreshes_running_line() {
        let mut tui = TuiLoop::new();
        tui.handle_event(Event::ToolStarted("execute_command".to_string()))
            .await
            .unwrap();
        assert_eq!(
            tui.app().current_tool().map(|s| s.as_str()),
            Some("execute_command")
        );
        tui.handle_event(Event::ToolProgress(
            "execute_command cargo test -p kod-tui".to_string(),
        ))
        .await
        .unwrap();
        assert_eq!(
            tui.app().current_tool().map(|s| s.as_str()),
            Some("execute_command cargo test -p kod-tui")
        );
    }

    #[tokio::test]
    async fn test_goal_set_show_clear() {
        let mut tui = TuiLoop::new();
        tui.handle_command("/goal ship the fix").await.unwrap();
        assert_eq!(tui.app().goal(), Some("ship the fix"));
        // Setting a goal dispatches it as the first prompt immediately.
        // Without a live engine (this test has none) dispatch_prompt
        // records the user message and returns without generating.
        // The input box is cleared by submit_input, and the goal text
        // appears in the chat as a user message.
        assert_eq!(tui.app().input(), "");
        let user_msg = tui
            .app()
            .messages()
            .iter()
            .find(|m| m.role == kod_types::MessageRole::User)
            .expect("goal text should be recorded as a user message");
        assert_eq!(user_msg.content, "ship the fix");

        tui.handle_command("/goal").await.unwrap();
        let last = tui.app().messages().last().unwrap();
        assert!(
            last.content.contains("ship the fix"),
            "got: {}",
            last.content
        );
        tui.handle_command("/goal clear").await.unwrap();
        assert_eq!(tui.app().goal(), None);
    }

    #[tokio::test]
    async fn test_cancel_command_stops_generation() {
        let mut tui = TuiLoop::new();
        tui.handle_command("/cancel").await.unwrap();
        let last = tui.app().messages().last().unwrap();
        assert!(
            last.content.contains("Nothing is running"),
            "got: {}",
            last.content
        );
        tui.app_mut().begin_generation();
        tui.handle_command("/cancel").await.unwrap();
        assert!(!tui.app().is_generating());
    }

    #[tokio::test]
    async fn test_steer_command_without_run_explains() {
        let mut tui = TuiLoop::new();
        tui.handle_command("/steer go left").await.unwrap();
        let last = tui.app().messages().last().unwrap();
        assert!(
            last.content.contains("Nothing is running"),
            "got: {}",
            last.content
        );
    }

    #[tokio::test]
    async fn test_enter_on_compact_compacts_and_announces() {
        let mut tui = TuiLoop::new();
        tui.app_mut().set_input_mode(InputMode::Insert);
        for i in 0..25 {
            tui.app_mut().set_input(format!("msg {i}"));
            tui.app_mut().submit_input();
        }
        tui.app_mut().set_input("/compact".to_string());
        assert!(tui.app().show_completions());
        tui.handle_event(Event::Key(KeyCode::Enter)).await.unwrap();
        let last = tui.app().messages().last().unwrap();
        assert!(last.content.contains("Compacted"), "got: {}", last.content);
    }

    #[tokio::test]
    async fn test_fail_generation_clears_running_tool_line() {
        let mut tui = TuiLoop::new();
        tui.app_mut().begin_generation();
        tui.handle_event(Event::ToolStarted("execute_command".to_string()))
            .await
            .unwrap();
        assert!(tui.app().current_tool().is_some());
        tui.handle_event(Event::Error("boom".to_string()))
            .await
            .unwrap();
        // The running line must not stick around for later messages.
        assert_eq!(tui.app().current_tool(), None);
        assert!(
            tui.app()
                .tool_executions()
                .iter()
                .all(|e| e.status != crate::app::ToolStatus::Running),
            "no phantom running entries"
        );
    }

    #[tokio::test]
    async fn test_tool_completed_with_header_resolves_running_entry() {
        let mut tui = TuiLoop::new();
        tui.handle_event(Event::ToolStarted("execute_command".to_string()))
            .await
            .unwrap();
        tui.handle_event(Event::ToolCompleted(
            "execute_command command=cargo test".to_string(),
            "ok".to_string(),
        ))
        .await
        .unwrap();
        assert_eq!(tui.app().current_tool(), None);
        let running = tui
            .app()
            .tool_executions()
            .iter()
            .filter(|e| e.status == crate::app::ToolStatus::Running)
            .count();
        assert_eq!(running, 0, "header must match the running entry");
        let last = tui.app().messages().last().unwrap();
        assert!(
            last.content.contains("execute_command"),
            "got: {}",
            last.content
        );
    }

    #[tokio::test]
    async fn test_live_done_marker_stamps_duration_and_fallback_keeps_it() {
        let mut tui = TuiLoop::new();
        tui.handle_event(Event::ToolStarted("execute_command".to_string()))
            .await
            .unwrap();
        // Live completion arrives first (pump delivers the done-marker).
        tui.handle_event(Event::ToolCompletedWithDuration(
            "execute_command command=cargo test".to_string(),
            "ok".to_string(),
            1340,
        ))
        .await
        .unwrap();
        let row = tui
            .app()
            .messages()
            .iter()
            .rev()
            .find(|m| m.content.contains("execute_command"))
            .unwrap();
        assert!(row.content.contains("1.3s"), "got: {}", row.content);
        // Task-end fallback arrives after: it must not strip the duration.
        tui.handle_event(Event::ToolCompleted(
            "execute_command command=cargo test".to_string(),
            "ok".to_string(),
        ))
        .await
        .unwrap();
        let row = tui
            .app()
            .messages()
            .iter()
            .rev()
            .find(|m| m.content.contains("execute_command"))
            .unwrap();
        assert!(row.content.contains("1.3s"), "got: {}", row.content);
        assert!(row.content.contains("ok"), "got: {}", row.content);
        let running = tui
            .app()
            .tool_executions()
            .iter()
            .filter(|e| e.status == crate::app::ToolStatus::Running)
            .count();
        assert_eq!(running, 0);
    }

    #[tokio::test]
    async fn test_engine_done_marker_parses_into_duration_event() {
        // The exact chunk the engine sends must survive the pump parsing.
        let chunk = kod_core::engine::tool_done_marker("read_file path=main.rs", "12 lines", 42);
        let (h, s, ms) = kod_core::engine::parse_tool_done(&chunk).expect("must parse");
        assert_eq!((h, s, ms), ("read_file path=main.rs", "12 lines", 42));
    }

    #[tokio::test]
    async fn test_task_end_does_not_reprint_flushed_text_as_giant_bubble() {
        // Screenshot repro: text streams, a tool runs, more text streams,
        // then the task ends with ToolCompleted + ResponseComplete carrying
        // the engine's FULL reply text. The end must not reprint everything
        // as one giant trailing bubble — the per-round bubbles already
        // showed it.
        let mut tui = TuiLoop::new();
        tui.app_mut().begin_generation();
        tui.handle_event(Event::ResponseChunk("before text ".to_string()))
            .await
            .unwrap();
        tui.handle_event(Event::ToolStarted("read_file".to_string()))
            .await
            .unwrap();
        tui.handle_event(Event::ResponseChunk("after text".to_string()))
            .await
            .unwrap();
        tui.handle_event(Event::ToolCompleted(
            "read_file path=main.rs".to_string(),
            "12 lines".to_string(),
        ))
        .await
        .unwrap();
        // Engine fallback = concatenation of every round's text.
        tui.handle_event(Event::ResponseComplete(
            "before text after text".to_string(),
        ))
        .await
        .unwrap();
        let bodies: Vec<&str> = tui
            .app()
            .messages()
            .iter()
            .map(|m| m.content.as_str())
            .collect();
        assert_eq!(bodies.len(), 3, "bubble/tool/bubble, got: {bodies:?}");
        assert!(bodies[0].contains("before text"), "got: {bodies:?}");
        assert!(bodies[1].contains("read_file"), "got: {bodies:?}");
        assert!(bodies[2].contains("after text"), "got: {bodies:?}");
    }

    #[tokio::test]
    async fn test_tool_start_flushes_streamed_text_into_own_bubble() {
        let mut tui = TuiLoop::new();
        tui.app_mut().begin_generation();
        tui.handle_event(Event::ResponseChunk("before text ".to_string()))
            .await
            .unwrap();
        tui.handle_event(Event::ToolStarted("read_file".to_string()))
            .await
            .unwrap();
        // The pre-tool text must already be its own assistant message —
        // not merged into whatever streams after the call — and the tool
        // row streams live right below it.
        assert_eq!(tui.app().messages().len(), 2);
        assert!(tui.app().messages()[0].content.contains("before text"));
        assert!(tui.app().messages()[1].content.contains("read_file"));
        assert!(tui.app().current_response().is_empty());
        tui.handle_event(Event::ToolCompleted(
            "read_file path=main.rs".to_string(),
            "12 lines".to_string(),
        ))
        .await
        .unwrap();
        tui.handle_event(Event::ResponseChunk("after text".to_string()))
            .await
            .unwrap();
        tui.handle_event(Event::ResponseComplete(String::new()))
            .await
            .unwrap();
        let bodies: Vec<&str> = tui
            .app()
            .messages()
            .iter()
            .map(|m| m.content.as_str())
            .collect();
        assert_eq!(bodies.len(), 3, "bubble/tool/bubble, got: {bodies:?}");
        assert!(bodies[0].contains("before text"), "got: {bodies:?}");
        assert!(bodies[1].contains("read_file"), "got: {bodies:?}");
        assert!(bodies[2].contains("after text"), "got: {bodies:?}");
    }
}
