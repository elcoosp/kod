//! Main TUI loop - coordinates rendering and event handling.

use crate::{
    app::{AppMode, ConfirmKind, InputMode, KodApp},
    event::{Event, EventHandler, KeyCode},
    keybindings::{KeyAction, load_bindings},
    ui::{
        AgentPanelWidget, ApprovalWidget, ChatWidget, CompletionsWidget, HeaderWidget, HelpWidget,
        InputWidget, QuestionWidget, StatusWidget,
    },
};
use kod_config::{KodConfig, LlmConfig};
use kod_core::{KodEngine, RouterConfig};
use kod_error::{KodError, Result};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
};
use std::io::Stdout;
use std::sync::Arc;
use std::time::Duration;

/// Help text for the `/help` command.
///
/// Kept in sync with `crate::app::SLASH_COMMANDS` by
/// `test_slash_help_lists_every_command` — adding a command to
/// `SLASH_COMMANDS` without updating this string fails the test, so
/// the help output and the `/` autocomplete cannot drift apart.
const SLASH_HELP: &str = "Commands:\n/help — show this help\n/clear — clear chat (asks confirm)\n/undo — restore last /clear\n/edit — load your last message back into the input for editing (also `e`)\n/model [<name>] — switch model; no argument lists the server's models\n/skills — list loaded skills\n/goal <text> — set a goal the agent works toward until GOAL MET (/goal clear to stop)\n/steer <instruction> — redirect the running prompt after its current tool call\n/cancel — stop the running prompt (also Esc or Ctrl+C while it runs)\n/compact — compact session history now\n/retry — resend the last prompt (also `r`)\n/search [<text>] — search chat (n/N next/prev, Esc clears)\n/copy — copy last assistant reply to clipboard (also `y`)\n/theme [dark|light] — cycle or set theme\n/tools — toggle tool-output visibility (also `t`)\n/debug last-prompt — write the last prompt sent to the model into ~/.kod/last_prompt.txt\n/debug tokens — show the token accounting breakdown for this session\n/doctor — print a diagnostics report (same as `kod doctor`)\n/init — onboarding info: config path, model profiles, next steps\n/regenerate — regenerate the last assistant reply\n/delete — remove the last user+assistant exchange\n/export [path] — export session as markdown (stdout when no path)\n/rollback [id] — restore a file from a checkpoint (newest when no id)\n/checkpoints — list file checkpoints for this project\n/swarm <goal> — run N agents: decompose, run concurrently, merge\n/quit — quit kod\n/check [<file>] — project check (LSP for a file, compiler for the whole workspace)\n/todo-add <text> — add a todo item to the session list\n/fork [label] — save the current chat as a restorable fork\n/reset — reset transient state: input, search, expansions, attachments\n/git-status — git status --porcelain=v2 in the current directory\n/stats — per-session statistics: roles, tools, tokens, elapsed\n/clearall — clear chat + long-term memory + checkpoints (asks for confirmation)\n/whoami — session summary: model, skills, context, paths\n/summarize — ask the model to summarize the session so far\n/grep <regex> — regex search the chat history\n/system <text> — override the system prompt for this session\n/branch [label] — drop a branch-point marker in the chat\n/load <path> — load a JSON session file\n/save <path> — save session markdown to a file\n/raw — print the last assistant reply raw (no decoration)\n/refine <instruction> — refine the last assistant reply\n/attach <path> — attach a file to the next prompt\n/diff — show the most recent file change (from checkpoints)\n/last-prompt — write the most recent prompt to ~/.kod/last_prompt.txt\n/context — visualize context window usage and session totals\n/memory [search <q> | delete <id> | clear] — long-term memory store\n/map [max-chars] — repository map (top-level symbols per file)\n\nWhile a prompt runs, typing + Enter steers it (same as /steer).\nKeys: i insert · j/k or wheel scrolls · q quit · PgUp/PgDn/Home/End · g/G top/bottom · t toggle tools · o expand · y copy · r retry · u undo · f search · ? help · Esc cancel — hold Option/Shift to select text";

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
    /// Char → action map for normal-mode single-key commands. Loaded at
    /// construction from `~/.config/kod/tui_keys.toml` and a project-
    /// local `.kod-keys.toml` (see [`crate::keybindings`]); a missing or
    /// corrupt file falls back to the built-in defaults. Tests override
    /// it via [`TuiLoop::set_keybindings`].
    keybindings: std::collections::HashMap<char, KeyAction>,
    /// Whether this loop reads and writes `~/.kod/tui_history.json`.
    ///
    /// Set to true only by [`TuiLoop::run`] — the production entry
    /// point. Tests drive `handle_event`/`dispatch_prompt` directly
    /// without calling `run`, so they neither touch the user's real
    /// history file nor observe a stale one. The load/persist helpers
    /// on `KodApp` exist and are correct; they simply had no caller
    /// before this — the doc comment on the persistence module
    /// promised cross-restart history that did not happen.
    persist_history: bool,
    /// When true, shell commands require the platform sandbox.
    /// Set from the `--sandbox` CLI flag before `run`.
    sandbox_required: bool,
    /// CLI `--preset` value, if any. Applied when the policy engine
    /// loads in `init_engine`; overrides the config and project
    /// policy layers.
    cli_preset: Option<String>,
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
            keybindings: load_bindings(),
            persist_history: false,
            sandbox_required: false,
            cli_preset: None,
        }
    }

    /// Replace the active keybinding map. Used by tests; production
    /// callers load the map once in [`TuiLoop::new`].
    pub fn set_keybindings(&mut self, bindings: std::collections::HashMap<char, KeyAction>) {
        self.keybindings = bindings;
    }

    /// When true, `init_engine` requires the platform sandbox for shell
    /// commands. Set from the `--sandbox` CLI flag before `run`.
    pub fn set_sandbox_mode(&mut self, required: bool) {
        self.sandbox_required = required;
    }

    /// Set the CLI `--preset` value. Applied by `init_engine` when the
    /// policy engine is built. `None` means "use the config/project
    /// layers only".
    pub fn set_cli_preset(&mut self, preset: Option<String>) {
        self.cli_preset = preset;
    }

    /// Set up the engine with the OpenAI-compatible provider
    pub async fn init_engine(&mut self, model: Option<String>) -> Result<()> {
        let config = KodConfig::load_default()?;
        let model_name = model.unwrap_or_else(|| config.llm.default_endpoint().model.clone());

        // Compute skills_dirs before `config.llm` is moved into
        // self.llm_config below — skills_dirs() borrows &self.config, and
        // the move would make that borrow illegal.
        let skills_dirs = config.skills_dirs()?;

        // The meter + compaction threshold must use the real window from
        // config (e.g. 8k for a small local model), not DEFAULT_CONTEXT_LIMIT.
        self.app.set_context_limit(config.llm.default_endpoint().context_window);

        // History budget: roughly three chars per token of the model's
        // window. The engine clamps anything below its floor, so a tiny
        // or placeholder context_window cannot produce an engine that
        // forgets every turn.
        let history_budget = config.llm.default_endpoint().context_window.saturating_mul(3);

        // KOD_TEST_DB isolates integration tests from a live session's
        // database. When unset, the config's `memory.scope` decides:
        // global `~/.kod/data/kod.redb` (the default) or per-project
        // `<cwd>/.kod/memory.redb`.
        let db_path = match std::env::var("KOD_TEST_DB") {
            Ok(p) => std::path::PathBuf::from(p),
            Err(_) => config.memory_db_path()?,
        };
        let _ = std::fs::create_dir_all(db_path.parent().unwrap());

        // Propagate the model's context window to the router so its
        // memory manager sizes its own budget from the same number the
        // engine uses for history.
        let router_config = RouterConfig {
            context_window: config.llm.default_endpoint().context_window,
            short_term_capacity: config.memory.short_term_capacity,
            ..RouterConfig::default()
        };
        let engine = KodEngine::new(router_config, db_path)?;
        engine.set_history_budget(history_budget);

        let (registry, default_model, routing) =
        kod_core::build_registry(&config.llm, Some(&model_name))?;
    engine
        .set_registry(registry, default_model, routing)
        .await;
        engine.set_hooks(config.hooks.clone());
        engine.set_network_access(config.llm.network_access);
        self.app
            .set_network_access_enabled(config.llm.network_access);
        engine.set_auto_check(config.tools.auto_check);
    engine.set_auto_lsp(config.tools.auto_lsp);
        if self.sandbox_required {
            engine.set_sandbox_mode(kod_tools::context::SandboxMode::Require);
        }
        kod_core::mcp_adapters::install_from_config(&engine, &config).await;

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
            // Hot reload: watch each existing skills directory so a
            // skill file added or edited mid-session appears in the
            // matcher without a restart. Gated by the config flag;
            // watching is cheap but every session that has it off
            // should not pay for the OS watcher.
            if config.skills.enable_hot_reload {
                for dir in &skills_dirs {
                    if dir.is_dir()
                        && let Err(e) = engine.enable_hot_reload(dir).await
                    {
                        tracing::warn!(
                            dir = %dir.display(),
                            error = %e,
                            "could not enable skill hot reload"
                        );
                    }
                }
            }
            let loaded: Vec<String> = engine.loaded_skill_names().await;
            self.app.set_loaded_skills(loaded);
        }

        // Load model list from provider so /model tab-completion is
        // useful. Best-effort: a failure here just means no
        // completion candidates, which /model (no args) will later
        // report explicitly when the user asks.
        if let Some(engine) = &self.engine {
            let models = engine.list_models().await.unwrap_or_default();
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

    /// Install an already-built engine. `init_engine` does the
    /// construction from config; this is the injection point for a
    /// caller (a test, an embedder) that built one itself.
    pub fn set_engine(&mut self, engine: std::sync::Arc<KodEngine>) {
        self.engine = Some(engine);
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
        let mut terminal = Terminal::new(backend)
            .map_err(|e| KodError::Internal(format!("Failed to create terminal: {}", e)))?;

        // Hide the terminal's own cursor. InputWidget draws an inline
        // `▌` at the current position, and leaving the real cursor
        // visible produced two carets on screen — one at the input
        // box and one wherever ratatui last placed the hardware cursor.
        // `restore_terminal` calls `show_cursor` on the way out.
        let _ = terminal.hide_cursor();

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

        // Load persisted prompt history and arm the save path.
        // KodApp::load_persistent_history reads
        // ~/.kod/tui_history.json (or KOD_TUI_STATE_DIR) best-effort —
        // a missing or corrupt file just means an empty history. The
        // matching persist_history_entry runs from dispatch_prompt
        // below, gated on the persist_history flag so tests do not
        // touch the real file.
        self.app.load_persistent_history();
        self.persist_history = true;

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
                    if self.app.is_approving() {
                        ApprovalWidget::new().render(&self.app, size, f.buffer_mut());
                    }
                    if self.app.is_asking() {
                        QuestionWidget::new().render(&self.app, size, f.buffer_mut());
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
                // Notify only for turns longer than 30 seconds — a
                // quick exchange does not deserve a bell.
                if let Some(elapsed) = self
                    .app
                    .notify_turn_complete(std::time::Duration::from_secs(30))
                {
                    self.app.push_system_message(&format!(
                        "(turn took {}s — press any key to focus)",
                        elapsed.as_secs(),
                    ));
                }
                // Snapshot the transcript after every completed turn.
                // The doc on KodApp::save_session has always claimed
                // "called on quit / after each assistant reply", but
                // only the quit path was wired — a crash mid-session
                // lost every turn since startup, not just the current
                // one. The write is atomic (temp + rename), so
                // persisting this often is safe; the cost is one small
                // JSON write per turn, negligible next to the model
                // call that just finished.
                //
                // Gated on persist_history so tests, which never call
                // TuiLoop::run, do not touch the user's real
                // ~/.kod/tui_session.json.
                if self.persist_history {
                    self.app.save_session();
                }
            }
            Event::TokenUsage(total) => {
                self.app.note_real_usage(total);
            }
            Event::SessionUsage {
                prompt_tokens,
                completion_tokens,
            } => {
                self.app
                    .note_session_usage(prompt_tokens, completion_tokens);
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
            Event::ApprovalRequested {
                id,
                tool_name,
                summary,
                diff,
            } => {
                self.app.set_pending_approval(
                    crate::app::PendingApproval {
                        id,
                        tool_name,
                        summary,
                        diff,
                    },
                );
            }
            Event::QuestionRequested {
                id,
                question,
                placeholder,
            } => {
                self.app.set_pending_question(
                    crate::app::PendingQuestion {
                        id,
                        question,
                        placeholder,
                    },
                );
            }
            Event::Cancelled => {
                self.gen_task = None;
                self.app.cancel_generation();
                // Cancelled turns are still worth persisting — the
                // partial assistant reply is kept (see
                // KodApp::cancel_generation), and the rest of the
                // session is unchanged. Same gate as ResponseComplete.
                if self.persist_history {
                    self.app.save_session();
                }
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
                // A failed turn appends a system message and settles
                // any running tool rows. Persist so a restart resumes
                // from the recorded error rather than the state
                // before it.
                if self.persist_history {
                    self.app.save_session();
                }
            }
            Event::SwarmDecomposed(subs) => {
                self.app.swarm_decomposed(&subs);
            }
            Event::SwarmAgentStarted { id, name, subtask } => {
                self.app.swarm_agent_started(id, &name, &subtask);
            }
            Event::SwarmAgentChunk { id, text } => {
                self.app.swarm_agent_chunk(&id, &text);
            }
            Event::SwarmAgentCompleted { id, result } => {
                self.app.swarm_agent_finished(&id, &result);
            }
            Event::SwarmAgentFailed { id, error } => {
                self.app.swarm_agent_failed(&id, &error);
            }
            Event::SwarmAgentWorktree { path, branch, .. } => {
                // Attach to the most recently started live agent that
                // has no worktree yet. The engine's WorktreeCreated
                // does not carry an id; a targeted id field is a
                // follow-up.
                let target = self
                    .app
                    .swarm_agents()
                    .iter()
                    .filter(|(_, v)| v.worktree.is_none() && !v.finished)
                    .map(|(id, _)| id.clone())
                    .last();
                if let Some(id) = target {
                    self.app.swarm_set_worktree(&id, path, branch);
                }
            }
            Event::SwarmAgentRetrying {
                id,
                attempt,
                max_attempts,
                previous_error,
            } => {
                self.app.swarm_set_retrying(
                    &id,
                    attempt,
                    max_attempts,
                    &previous_error,
                );
            }
            Event::SwarmConflict { file, agents } => {
                self.app.push_system_message(&format!(
                    "⚠ conflict: {} written by {}",
                    file,
                    agents.join(", ")
                ));
            }
            Event::SwarmMerging => {
                self.app.push_system_message("── merging swarm results ──");
            }
            Event::SwarmComplete(merged) => {
                self.gen_task = None;
                self.app.swarm_complete(&merged);
                if self.persist_history {
                    self.app.save_session();
                }
            }
            Event::SwarmError(e) => {
                self.gen_task = None;
                self.app.fail_generation(&e);
                if self.persist_history {
                    self.app.save_session();
                }
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

        // Prepend attached files. The `input` sent to the engine is
        // rewritten here; the display row was pushed by submit_input
        // above using the user's literal text, which is what the user
        // wants to see in the chat.
        // System prompt override wraps the outgoing message.
        let input_with_system = match self.app.session_system_prompt() {
            Some(sys) if !sys.is_empty() => format!(
                "[system] {sys}\n\n[user] {input}",
            ),
            _ => input.clone(),
        };
        let input = input_with_system;

        let attached = self.app.take_attachments();
        let input = if attached.is_empty() {
            input
        } else {
            let mut buf = String::with_capacity(input.len() + 512);
            for f in &attached {
                match std::fs::read_to_string(f) {
                    Ok(body) => {
                        let shown = if body.len() > 64 * 1024 {
                            format!("{}…\n[truncated]", &body[..64 * 1024])
                        } else {
                            body
                        };
                        buf.push_str(&format!(
                            "<file path=\"{}\">\n{}\n</file>\n",
                            f.display(),
                            shown,
                        ));
                    }
                    Err(e) => {
                        self.app.push_system_message(&format!(
                            "Could not read attachment {}: {}",
                            f.display(),
                            e
                        ));
                    }
                }
            }
            buf.push_str(&input);
            buf
        };

        // Remember the prompt for /retry (and the `r` key). The previous
        // code only set last_prompt from retry_generation itself, so
        // last_prompt() was always None on first use and /retry always
        // answered 'Nothing to retry'.
        self.app.set_last_prompt(&input);

        // Append the raw prompt to the cross-session history file, so
        // Up-arrow in the next session recalls what was typed this one.
        // Skipped in tests (persist_history is only true after run()),
        // and skipped for slash commands by living below the earlier
        // `input.trim_start().starts_with('/')` early return — a
        // recalled `/help` in the history would be noise next time.
        if self.persist_history {
            self.app.persist_history_entry(&input);
        }

        // Without an engine (e.g. in tests) the message is recorded and
        // nothing else happens.
        let Some(engine) = self.engine.clone() else {
            return Ok(());
        };

        // Mark the turn as started BEFORE counting the prompt.
        //
        // begin_generation resets `turn_has_real_usage`, which
        // note_prompt's estimate consults via note_usage. The previous
        // order (note_prompt, then begin_generation) meant that on any
        // turn after a turn that received a real TokenUsage total, the
        // flag was still true from the previous turn when note_prompt
        // ran, so the prompt's estimate was silently dropped and the
        // meter under-reported until the next TokenUsage arrived.
        //
        // begin_generation also arms the streaming accumulator, so
        // moving it up does not change event handling — it just
        // establishes "new turn" before anything contributes to the
        // turn's accounting.
        self.app.begin_generation();

        // Track session context after the turn boundary is set.
        self.app.note_prompt(&input);

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
                    if let Some((id, json)) =
                        kod_core::engine::parse_question(&chunk)
                    {
                        let req: kod_tools::ask::QuestionRequest =
                            serde_json::from_str(json).unwrap_or_else(|_| {
                                kod_tools::ask::QuestionRequest {
                                    question: "(unparseable question)".to_string(),
                                    placeholder: None,
                                }
                            });
                        let _ = event_tx_chunks
                            .send(Event::QuestionRequested {
                                id,
                                question: req.question,
                                placeholder: req.placeholder,
                            })
                            .await;
                    } else if let Some((id, json)) =
                        kod_core::engine::parse_tool_approval(&chunk)
                    {
                        let request: kod_core::engine::ApprovalRequest =
                            serde_json::from_str(json).unwrap_or_else(|_| {
                                kod_core::engine::ApprovalRequest {
                                    tool_name: "?".to_string(),
                                    arguments: serde_json::Value::Null,
                                    diff: None,
                                    summary: "(unparseable approval request)".to_string(),
                                }
                            });
                        let _ = event_tx_chunks
                            .send(Event::ApprovalRequested {
                                id,
                                tool_name: request.tool_name,
                                summary: request.summary,
                                diff: request.diff,
                            })
                            .await;
                    } else if let Some(tool) = kod_core::engine::parse_tool_start(&chunk) {
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
                        let _ = event_tx
                            .send(Event::SessionUsage {
                                prompt_tokens: usage.prompt_tokens,
                                completion_tokens: usage.completion_tokens,
                            })
                            .await;
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

    /// Start a swarm run in the background. Same shape as
    /// `dispatch_prompt`: a spawned task runs the runner, its events
    /// flow through the event channel, and Esc / Ctrl+C aborts the task
    /// via `gen_task`.
    async fn dispatch_swarm(&mut self, goal: String) -> Result<()> {
        let Some(engine) = self.engine.clone() else {
            self.app.push_system_message("Engine not initialized.");
            return Ok(());
        };
        if self.app.is_generating() {
            self.app.push_system_message(
                "A generation is already running — cancel it first (Esc or /cancel).",
            );
            return Ok(());
        }

        let config = kod_config::KodConfig::load_default()?;
        let n = config.swarm.max_agents;
        let merge = config.swarm.merge_results;

        self.app.begin_swarm();
        self.app.begin_generation();
        self.app.push_system_message(&format!(
            "Starting swarm ({} agents, merge {})",
            n,
            if merge { "on" } else { "off" }
        ));

        engine.clear_cancel();

        let event_tx = self.event_handler.sender();
        let handle = tokio::spawn(async move {
            let runner = match kod_core::SwarmRunner::new(engine, n, merge).await {
                Ok(r) => r,
                Err(e) => {
                    let _ = event_tx.send(Event::SwarmError(e.to_string())).await;
                    return;
                }
            };

            let (chunk_tx, mut chunk_rx) =
                tokio::sync::mpsc::channel::<kod_core::SwarmEvent>(128);
            let event_tx_pump = event_tx.clone();
            let pump = tokio::spawn(async move {
                while let Some(ev) = chunk_rx.recv().await {
                    let tui_ev = match ev {
                        kod_core::SwarmEvent::Decomposed(subs) => Event::SwarmDecomposed(
                            subs.iter()
                                .map(|s| (s.name.clone(), s.description.clone()))
                                .collect(),
                        ),
                        kod_core::SwarmEvent::AgentStarted { id, name, subtask } => {
                            Event::SwarmAgentStarted { id, name, subtask }
                        }
                        kod_core::SwarmEvent::AgentChunk { id, text, .. } => {
                            Event::SwarmAgentChunk { id, text }
                        }
                        kod_core::SwarmEvent::AgentCompleted { id, result, .. } => {
                            Event::SwarmAgentCompleted { id, result }
                        }
                        kod_core::SwarmEvent::AgentFailed { id, error, .. } => {
                            Event::SwarmAgentFailed { id, error }
                        }
                        kod_core::SwarmEvent::ConflictDetected { file, agents } => {
                            Event::SwarmConflict { file, agents }
                        }
                        kod_core::SwarmEvent::AgentRetrying {
                            id: _,
                            name,
                            attempt,
                            max_attempts,
                            previous_error,
                        } => Event::AgentMessage(
                            "swarm".to_string(),
                            format!(
                                "{} retrying ({}/{}): {}",
                                name, attempt, max_attempts, previous_error,
                            ),
                        ),
                        kod_core::SwarmEvent::Merging => Event::SwarmMerging,
                        kod_core::SwarmEvent::WorktreeCreated {
                            agent_name,
                            path,
                            branch,
                        } => Event::AgentMessage(
                            "swarm".to_string(),
                            format!(
                                "{}: worktree {} (branch {})",
                                agent_name,
                                path.display(),
                                branch,
                            ),
                        ),
                        kod_core::SwarmEvent::WorktreesMerged {
                            merged,
                            conflicted,
                            failed,
                        } => {
                            let summary = if conflicted.is_empty()
                                && failed.is_empty()
                            {
                                format!(
                                    "worktrees merged: {} ok",
                                    merged.len()
                                )
                            } else {
                                format!(
                                    "worktrees merged: {} ok, {} conflict(s), \
                                     {} failed",
                                    merged.len(),
                                    conflicted.len(),
                                    failed.len(),
                                )
                            };
                            Event::AgentMessage("swarm".to_string(), summary)
                        }
                    };
                    let _ = event_tx_pump.send(tui_ev).await;
                }
            });

            let result = runner.run(&goal, &chunk_tx).await;
            drop(chunk_tx);
            let _ = pump.await;

            match result {
                Ok(resp) => {
                    let _ = event_tx.send(Event::SwarmComplete(resp.merged)).await;
                }
                Err(e) => {
                    let _ = event_tx.send(Event::SwarmError(e.to_string())).await;
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
        // Restore into the input box and dispatch again. No need to
        // re-set last_prompt: dispatch_prompt overwrites it with the
        // same text on the way through.
        self.app.set_input(prompt);
        // Box::pin breaks the dispatch → handle_command → retry →
        // dispatch recursive future cycle (Rust requires indirection).
        Box::pin(self.dispatch_prompt()).await
    }

    /// Execute a `/` command (input already recorded as a user message).
    /// Execute a slash command as if the user typed it. Public so an
    /// embedder (or an integration test) can drive command dispatch
    /// without going through the event loop.
    pub async fn handle_command(&mut self, input: &str) -> Result<()> {
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
                None => self.show_and_refresh_models().await?,
            },
            "/skills" => {
                // `/skills <name>` prints the full instructions of one
                // skill. `/skills` (no arg) keeps the existing listing.
                if let Some(name) = parts.next() {
                    let Some(engine) = &self.engine else {
                        self.app.push_system_message("Engine not initialized.");
                        return Ok(());
                    };
                    // Find the skill by name from the router's matcher.
                    let details = engine.loaded_skill_details().await;
                    let found = details.iter().find(|(n, _)| n == name).cloned();
                    match found {
                        Some((n, d)) => {
                            // Read the file for the full content.
                            let config = KodConfig::load_default().ok();
                            let mut body: Option<String> = None;
                            if let Some(cfg) = &config {
                                if let Ok(dirs) = cfg.skills_dirs() {
                                    for dir in dirs.iter() {
                                        if !dir.is_dir() {
                                            continue;
                                        }
                                        for entry in walkdir::WalkDir::new(dir)
                                            .follow_links(false)
                                            .into_iter()
                                            .filter_map(|e| e.ok())
                                        {
                                            if !entry.file_type().is_file() {
                                                continue;
                                            }
                                            if entry.path().extension().and_then(|s| s.to_str()) != Some("md") {
                                                continue;
                                            }
                                            let stem = entry.path().file_stem().and_then(|s| s.to_str()).unwrap_or("");
                                            if stem == n {
                                                if let Ok(text) = std::fs::read_to_string(entry.path()) {
                                                    body = Some(text);
                                                }
                                                break;
                                            }
                                        }
                                        if body.is_some() {
                                            break;
                                        }
                                    }
                                }
                            }
                            let text = body.unwrap_or_else(|| format!("(description) {}", d));
                            self.app.push_system_message(&format!(
                                "Skill {}\n\n{}",
                                n,
                                text,
                            ));
                        }
                        None => self.app.push_system_message(&format!(
                            "No skill named {:?}. Run /skills to list.",
                            name,
                        )),
                    }
                    return Ok(());
                }

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
                    // Open the type-ahead search bar. From here the
                    // user's keystrokes go into the query (see
                    // `handle_key`'s `is_editing_search` branch),
                    // Enter commits, Escape clears. This is the
                    // design `begin_search` was originally written
                    // for; the previous workaround (prefill the input
                    // with "/search ") is gone.
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
                    let prev = self.app.theme_name().to_string();
                    let known = self.app.try_set_theme(name);
                    if known {
                        self.app
                            .push_system_message(&format!("Theme {prev} → {name}"));
                    } else {
                        self.app.push_system_message(&format!(
                            "Unknown theme '{}'. Known themes: dark, light. \
                             Fell back to dark. (Custom themes come from ~/.config/kod/theme.toml \
                             or a project-local .kod-theme.toml.)",
                            name
                        ));
                    }
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
            "/edit" => {
                // Same behaviour as the `e` keybinding: load the last
                // user message into the input box for editing. The
                // completion popup advertised /edit before this arm
                // existed, so picking it fell through to "Unknown
                // command: /edit" — a small lie caught by
                // test_slash_help_lists_every_command.
                if self.app.edit_last_message() {
                    self.app.set_input_mode(InputMode::Insert);
                    self.app.push_system_message(
                        "Loaded your last message for editing — press Enter to resend.",
                    );
                } else {
                    self.app
                        .push_system_message("Nothing to edit — no previous prompt.");
                }
            }
            "/debug" => match parts.next() {
                Some("last-prompt") | Some("last_prompt") => {
                    let Some(engine) = &self.engine else {
                        self.app.push_system_message("Engine not initialized.");
                        return Ok(());
                    };
                    match engine.last_prompt().await {
                        Some(prompt) => {
                            let path = dirs::home_dir()
                                .map(|h| h.join(".kod").join("last_prompt.txt"));
                            match path {
                                Some(p) => {
                                    if let Some(parent) = p.parent() {
                                        let _ = std::fs::create_dir_all(parent);
                                    }
                                    match std::fs::write(&p, prompt.as_bytes()) {
                                        Ok(()) => self.app.push_system_message(&format!(
                                            "Wrote last prompt ({} chars, {} lines) to {}\n\
                                             Inspect with: cat {}",
                                            prompt.len(),
                                            prompt.lines().count(),
                                            p.display(),
                                            p.display()
                                        )),
                                        Err(e) => self.app.push_system_message(&format!(
                                            "Could not write {}: {}",
                                            p.display(),
                                            e
                                        )),
                                    }
                                }
                                None => self
                                    .app
                                    .push_system_message("Could not determine home directory."),
                            }
                        }
                        None => self
                            .app
                            .push_system_message("No prompt has been sent yet this session."),
                    }
                }
                Some("tokens") => {
                    let label = self.app.context_label();
                    let used = self.app.context_tokens();
                    let limit = self.app.context_limit();
                    let pct = self.app.context_usage() * 100.0;
                    let inp = self.app.session_input_tokens();
                    let out = self.app.session_output_tokens();
                    let total = self.app.session_total_tokens();
                    let elapsed = self.app.elapsed_session();
                    let secs = elapsed.as_secs();
                    let elapsed_label = if secs < 60 {
                        format!("{secs}s")
                    } else if secs < 3600 {
                        format!("{}m{:02}s", secs / 60, secs % 60)
                    } else {
                        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
                    };
                    self.app.push_system_message(&format!(
                        "Token accounting\n\
                         \n\
                         Window (approx):\n\
                           used:  {} tokens\n\
                           limit: {} tokens\n\
                           fill:  {:.1}%\n\
                           label: {}\n\
                         \n\
                         Session totals (from provider usage):\n\
                           input:  {} tokens\n\
                           output: {} tokens\n\
                           total:  {} tokens\n\
                         \n\
                         Elapsed: {}",
                        used, limit, pct, label, inp, out, total, elapsed_label,
                    ));
                }
                _ => {
                    self.app.push_system_message(
                        "Usage: /debug last-prompt — writes the last prompt sent to the model into ~/.kod/last_prompt.txt\n\
                         Usage: /debug tokens — show the token accounting breakdown for this session",
                    );
                }
            },
            "/swarm" => {
                let rest: String = parts.collect::<Vec<_>>().join(" ");
                let goal = rest.trim();
                if goal.is_empty() {
                    self.app.push_system_message(
                        "Usage: /swarm <goal> — decomposes the goal into N agents, \
                         runs them concurrently, and merges the results. N comes \
                         from `swarm.max_agents` in the config (default 5).",
                    );
                } else {
                    self.dispatch_swarm(goal.to_string()).await?;
                }
            }
            "/rollback" => {
                let Some(engine) = &self.engine else {
                    self.app.push_system_message("Engine not initialized.");
                    return Ok(());
                };
                let Some(cp) = engine.checkpoints() else {
                    self.app.push_system_message(
                        "No checkpoint directory — a home directory is required.",
                    );
                    return Ok(());
                };
                match parts.next() {
                    Some(id) => match cp.restore(id) {
                        Ok(path) => self.app.push_system_message(&format!(
                            "Restored {} from checkpoint {}.",
                            path.display(),
                            id,
                        )),
                        Err(e) => self
                            .app
                            .push_system_message(&format!("Rollback failed: {e}")),
                    },
                    None => match cp.list() {
                        Ok(list) if list.is_empty() => self.app.push_system_message(
                            "No checkpoints yet. A checkpoint is written before each \
                             write_file or patch_file.",
                        ),
                        Ok(list) => {
                            let newest = &list[0];
                            match cp.restore(&newest.id) {
                                Ok(path) => self.app.push_system_message(&format!(
                                    "Restored {} from checkpoint {} ({}, {}).",
                                    path.display(),
                                    newest.id,
                                    if newest.existed { "modify" } else { "create" },
                                    newest.tool,
                                )),
                                Err(e) => self
                                    .app
                                    .push_system_message(&format!("Rollback failed: {e}")),
                            }
                        }
                        Err(e) => self
                            .app
                            .push_system_message(&format!("Could not list checkpoints: {e}")),
                    },
                }
            }
            "/checkpoints" => {
                let Some(engine) = &self.engine else {
                    self.app.push_system_message("Engine not initialized.");
                    return Ok(());
                };
                let Some(cp) = engine.checkpoints() else {
                    self.app.push_system_message(
                        "No checkpoint directory — a home directory is required.",
                    );
                    return Ok(());
                };
                match cp.list() {
                    Ok(list) if list.is_empty() => self.app.push_system_message(
                        "No checkpoints yet. A checkpoint is written before each \
                         write_file or patch_file.",
                    ),
                    Ok(list) => {
                        let mut msg =
                            format!("Checkpoints ({} total, newest first):\n", list.len());
                        for s in list.iter().take(20) {
                            let kind = if s.existed { "modify" } else { "create" };
                            msg.push_str(&format!(
                                "  {}  {:<7} {:<12} {}\n",
                                s.id,
                                kind,
                                s.tool,
                                s.path.display(),
                            ));
                        }
                        if list.len() > 20 {
                            msg.push_str(&format!(
                                "… and {} older — `kod checkpoint list` shows more.\n",
                                list.len() - 20,
                            ));
                        }
                        msg.push_str(
                            "\nRestore with /rollback <id>, or /rollback for the newest.",
                        );
                        self.app.push_system_message(&msg);
                    }
                    Err(e) => self
                        .app
                        .push_system_message(&format!("Could not list checkpoints: {e}")),
                }
            }
            "/regenerate" => {
                if self.app.is_generating() {
                    self.app.push_system_message(
                        "Wait for the current prompt to finish before regenerating.",
                    );
                    return Ok(());
                }
                match self.app.drop_last_exchange() {
                    Some(prompt) => {
                        self.app.set_input(prompt);
                        self.app.push_system_message(
                            "Last exchange removed — resending the same prompt.",
                        );
                        Box::pin(self.dispatch_prompt()).await?;
                    }
                    None => self.app.push_system_message("Nothing to regenerate."),
                }
            }
            "/delete" => {
                if self.app.is_generating() {
                    self.app.push_system_message(
                        "Wait for the current prompt to finish before deleting.",
                    );
                    return Ok(());
                }
                match self.app.drop_last_exchange() {
                    Some(_) => self
                        .app
                        .push_system_message("Last exchange removed."),
                    None => self.app.push_system_message("Nothing to delete."),
                }
            }
            "/memory" => {
                let config = match KodConfig::load_default() {
                    Ok(c) => c,
                    Err(e) => {
                        self.app
                            .push_system_message(&format!("Could not load config: {e}"));
                        return Ok(());
                    }
                };
                let path = match config.memory_db_path() {
                    Ok(p) => p,
                    Err(e) => {
                        self.app.push_system_message(&format!(
                            "Could not determine memory database path: {e}"
                        ));
                        return Ok(());
                    }
                };
                let manager = match kod_memory::MemoryManager::new(
                    path,
                    config.memory.short_term_capacity,
                ) {
                    Ok(m) => m,
                    Err(e) => {
                        self.app.push_system_message(&format!(
                            "Could not open memory database: {e}"
                        ));
                        return Ok(());
                    }
                };

                let sub = parts.next();
                match sub {
                    Some("search") => {
                        let query: String = parts.collect::<Vec<_>>().join(" ");
                        if query.trim().is_empty() {
                            self.app.push_system_message("Usage: /memory search <text>");
                        } else {
                            match manager.search(&query).await {
                                Ok(hits) if hits.is_empty() => self.app.push_system_message(
                                    &format!("No memory entries match {:?}.", query),
                                ),
                                Ok(hits) => {
                                    let mut msg = format!(
                                        "{} memory entr{} match {:?}:\n",
                                        hits.len(),
                                        if hits.len() == 1 { "y" } else { "ies" },
                                        query,
                                    );
                                    for e in hits.iter().take(30) {
                                        let short = &e.id.as_uuid().to_string()[..8];
                                        let one = e.content.lines().next().unwrap_or("");
                                        let shown = if one.chars().count() > 100 {
                                            let s: String = one.chars().take(100).collect();
                                            format!("{s}…")
                                        } else {
                                            one.to_string()
                                        };
                                        msg.push_str(&format!("  {}  {}\n", short, shown));
                                    }
                                    self.app.push_system_message(msg.trim_end());
                                }
                                Err(e) => self.app.push_system_message(&format!(
                                    "Search failed: {e}"
                                )),
                            }
                        }
                    }
                    Some("delete") => match parts.next() {
                        Some(prefix) => match manager.get_all_long_term().await {
                            Ok(all) => match all
                                .iter()
                                .find(|e| e.id.as_uuid().to_string().starts_with(prefix))
                            {
                                Some(entry) => {
                                    match manager
                                        .remove(
                                            kod_types::MemoryType::LongTerm,
                                            &entry.id,
                                        )
                                        .await
                                    {
                                        Ok(()) => self.app.push_system_message(
                                            &format!("Deleted memory entry {}.", &entry.id.as_uuid().to_string()[..8]),
                                        ),
                                        Err(e) => self.app.push_system_message(&format!(
                                            "Delete failed: {e}"
                                        )),
                                    }
                                }
                                None => self.app.push_system_message(&format!(
                                    "No memory entry with id prefix {:?}.",
                                    prefix
                                )),
                            },
                            Err(e) => self.app.push_system_message(&format!(
                                "Could not read memory database: {e}"
                            )),
                        },
                        None => self.app.push_system_message("Usage: /memory delete <id>"),
                    },
                    Some("clear") => {
                        match manager.get_all_long_term().await {
                            Ok(all) => {
                                let n = all.len();
                                for e in &all {
                                    let _ = manager
                                        .remove(
                                            kod_types::MemoryType::LongTerm,
                                            &e.id,
                                        )
                                        .await;
                                }
                                self.app.push_system_message(&format!(
                                    "Cleared {} memory entr{}.",
                                    n,
                                    if n == 1 { "y" } else { "ies" },
                                ));
                            }
                            Err(e) => self.app.push_system_message(&format!(
                                "Could not read memory database: {e}"
                            )),
                        }
                    }
                    _ => {
                        // No subcommand: list entries.
                        match manager.get_all_long_term().await {
                            Ok(all) if all.is_empty() => self.app.push_system_message(
                                "No long-term memory entries. Add some with the memory tools.",
                            ),
                            Ok(all) => {
                                let mut msg = format!(
                                    "Long-term memory ({} entries):\n",
                                    all.len(),
                                );
                                for e in all.iter().take(30) {
                                    let short = &e.id.as_uuid().to_string()[..8];
                                    let one = e.content.lines().next().unwrap_or("");
                                    let shown = if one.chars().count() > 100 {
                                        let s: String = one.chars().take(100).collect();
                                        format!("{s}…")
                                    } else {
                                        one.to_string()
                                    };
                                    msg.push_str(&format!("  {}  {}\n", short, shown));
                                }
                                if all.len() > 30 {
                                    msg.push_str(&format!("… and {} more.", all.len() - 30));
                                }
                                msg.push_str(
                                    "\nSubcommands: /memory search <q>, /memory delete <id>, /memory clear",
                                );
                                self.app.push_system_message(msg.trim_end());
                            }
                            Err(e) => self.app.push_system_message(&format!(
                                "Could not read memory database: {e}"
                            )),
                        }
                    }
                }
            }
            "/context" => {
                let used = self.app.context_tokens();
                let limit = self.app.context_limit();
                let pct = self.app.context_usage() * 100.0;
                let inp = self.app.session_input_tokens();
                let out = self.app.session_output_tokens();
                let total = self.app.session_total_tokens();
                let msg_count = self.app.messages().len();
                let assistant_count = self.app
                    .messages()
                    .iter()
                    .filter(|m| matches!(m.role, kod_types::MessageRole::Assistant))
                    .count();
                let tool_count = self.app
                    .messages()
                    .iter()
                    .filter(|m| matches!(m.role, kod_types::MessageRole::Tool))
                    .count();
                let warning = self.app.context_warning().unwrap_or("");
                let mut msg = String::new();
                msg.push_str("Context window\n");
                msg.push_str(&format!("  approx used:  {} tokens\n", used));
                msg.push_str(&format!("  limit:        {} tokens\n", limit));
                msg.push_str(&format!("  fill:         {:.1}%\n", pct));
                msg.push_str(&format!("  label:        {}\n", self.app.context_label()));
                if !warning.is_empty() {
                    msg.push_str(&format!("  warning:      {}\n", warning));
                }
                msg.push_str("\nSession totals (from provider usage)\n");
                msg.push_str(&format!("  input:        {} tokens\n", inp));
                msg.push_str(&format!("  output:       {} tokens\n", out));
                msg.push_str(&format!("  total:        {} tokens\n", total));
                msg.push_str("\nMessage counts\n");
                msg.push_str(&format!("  total:        {}\n", msg_count));
                msg.push_str(&format!("  assistant:    {}\n", assistant_count));
                msg.push_str(&format!("  tool:         {}\n", tool_count));
                msg.push_str(&format!(
                    "\nAuto-compact fires at {}% of the window.\n",
                    4 * 100 / 5
                ));
                msg.push_str("Force with /compact. Reset with /clear.");
                self.app.push_system_message(&msg);
            }
            "/last-prompt" => {
                // Shortcut for /debug last-prompt.
                Box::pin(self.handle_command("/debug last-prompt")).await?;
            }
            "/diff" => {
                let Some(engine) = &self.engine else {
                    self.app.push_system_message("Engine not initialized.");
                    return Ok(());
                };
                let Some(cp) = engine.checkpoints() else {
                    self.app.push_system_message(
                        "No checkpoint directory — a home directory is required.",
                    );
                    return Ok(());
                };
                match cp.list() {
                    Ok(list) if list.is_empty() => self.app.push_system_message(
                        "No file diffs to show. A checkpoint is written before each \
                         write_file or patch_file.",
                    ),
                    Ok(list) => {
                        // Show the most recent checkpoint's diff. If the
                        // snapshot was for a create, the current file is
                        // the new content.
                        let newest = &list[0];
                        let current = std::fs::read_to_string(&newest.path).unwrap_or_default();
                        let diff = kod_tools::patch::render_unified_diff(
                            &newest.content,
                            &current,
                            &newest.path.display().to_string(),
                        );
                        if diff.trim().is_empty() {
                            self.app.push_system_message(&format!(
                                "Newest checkpoint ({}): {} — no change since.",
                                newest.id,
                                newest.path.display(),
                            ));
                        } else {
                            self.app.push_system_message(&format!(
                                "Most recent file change ({}: {} · {})\n\n{}",
                                newest.id,
                                newest.tool,
                                newest.path.display(),
                                diff,
                            ));
                        }
                    }
                    Err(e) => self.app.push_system_message(&format!(
                        "Could not read checkpoints: {e}",
                    )),
                }
            }
            "/attach" => {
                match parts.next() {
                    None => {
                        // No argument: list current attachments and the
                        // usage line.
                        let attached = self.app.attached_files();
                        if attached.is_empty() {
                            self.app.push_system_message(
                                "Usage: /attach <path> — attach a file to the next prompt. \
                                 \nClear with /attach clear. Multiple files supported.",
                            );
                        } else {
                            let mut msg = format!(
                                "Attached files ({}):\n",
                                attached.len(),
                            );
                            for f in attached {
                                msg.push_str(&format!("  {}\n", f.display()));
                            }
                            msg.push_str("\n/attach clear to remove, or send your next prompt.");
                            self.app.push_system_message(&msg);
                        }
                    }
                    Some("clear") => {
                        let n = self.app.attached_files().len();
                        self.app.clear_attachments();
                        self.app
                            .push_system_message(&format!("Cleared {} attachment(s).", n));
                    }
                    Some(path_str) => {
                        let path = std::path::PathBuf::from(path_str);
                        if self.app.attach_file(path.clone()) {
                            self.app.push_system_message(&format!(
                                "Attached {}. It will be sent with the next prompt.",
                                path.display()
                            ));
                        } else {
                            self.app.push_system_message(&format!(
                                "Could not attach {}: not a readable file.",
                                path.display()
                            ));
                        }
                    }
                }
            }
            "/refine" => {
                if self.app.is_generating() {
                    self.app.push_system_message(
                        "Wait for the current prompt to finish before refining.",
                    );
                    return Ok(());
                }
                let instruction: String = parts.collect::<Vec<_>>().join(" ");
                if instruction.trim().is_empty() {
                    self.app.push_system_message(
                        "Usage: /refine <instruction> — refine the last assistant reply \
                         given this instruction. Example: /refine make it shorter",
                    );
                    return Ok(());
                }
                let Some(last_assistant) = self.app.last_assistant_text() else {
                    self.app.push_system_message(
                        "Nothing to refine — no assistant reply yet.",
                    );
                    return Ok(());
                };
                let prior = last_assistant.to_string();
                let prompt = format!(
                    "Here is your previous reply:\n\n{prior}\n\n\
                     Refine it according to this instruction:\n\n{instruction}",
                );
                // Drop the last assistant reply so we do not create a
                // chain of unrefined → refined → refined again.
                let _ = self.app.drop_last_exchange();
                self.app.set_input(prompt);
                self.app
                    .push_system_message("Refining the last reply…");
                Box::pin(self.dispatch_prompt()).await?;
            }
            "/raw" => {
                // Print the last assistant reply with no decoration —
                // no markdown fences, no bubble frame. Useful when
                // copy-pasting code from a rendered reply.
                let text = self.app.last_assistant_text().map(|s| s.to_string());
                match text {
                    Some(t) => {
                        println!();
                        println!("{}", t);
                        println!();
                        self.app.push_system_message(
                            "(raw reply printed to stdout — select with your terminal)",
                        );
                    }
                    None => self.app.push_system_message(
                        "No assistant reply yet.",
                    ),
                }
            }
            "/save" => {
                let path = match parts.next() {
                    Some(p) => std::path::PathBuf::from(p),
                    None => {
                        self.app.push_system_message("Usage: /save <path>");
                        return Ok(());
                    }
                };
                if let Some(parent) = path.parent()
                    && !parent.as_os_str().is_empty()
                {
                    let _ = std::fs::create_dir_all(parent);
                }
                let markdown = self.app.export_markdown();
                match std::fs::write(&path, markdown.as_bytes()) {
                    Ok(()) => self.app.push_system_message(&format!(
                        "Saved session ({} bytes) to {}",
                        markdown.len(),
                        path.display(),
                    )),
                    Err(e) => self.app
                        .push_system_message(&format!("Save failed: {e}")),
                }
            }
            "/load" => {
                let path = match parts.next() {
                    Some(p) => std::path::PathBuf::from(p),
                    None => {
                        self.app.push_system_message("Usage: /load <path>");
                        return Ok(());
                    }
                };
                match std::fs::read_to_string(&path) {
                    Ok(_) => {
                        // Only the JSON serialized session round-trips;
                        // markdown is one-way. Try JSON first.
                        match std::fs::read_to_string(&path)
                            .ok()
                            .and_then(|s| serde_json::from_str::<Vec<crate::app::Message>>(&s).ok())
                        {
                            Some(msgs) => {
                                let n = msgs.len();
                                self.app.replace_messages(msgs);
                                self.app.push_system_message(&format!(
                                    "Loaded {} message(s) from {}",
                                    n,
                                    path.display(),
                                ));
                            }
                            None => self.app.push_system_message(
                                "That file is not a JSON session file. Use /export --json to produce one.",
                            ),
                        }
                    }
                    Err(e) => self.app
                        .push_system_message(&format!("Load failed: {e}")),
                }
            }
            "/branch" => {
                // Drop a system marker in the chat. A subsequent
                // /save <path> captures everything up to this point,
                // making the marker a "branch from here" anchor for a
                // manual workflow. Automatic branch-state capture is
                // a follow-up.
                let n = self.app.messages().len();
                let label = parts
                    .next()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| format!("branch-{}", n));
                self.app.push_system_message(&format!(
                    "⤵ branch point: {} ({} messages so far). Use /save <path> to capture this state.",
                    label, n,
                ));
            }
            "/system" => {
                let rest: String = parts.collect::<Vec<_>>().join(" ");
                let rest = rest.trim();
                if rest.eq_ignore_ascii_case("clear") || rest.eq_ignore_ascii_case("off") {
                    self.app.clear_session_system_prompt();
                    self.app
                        .push_system_message("System prompt override cleared.");
                } else if rest.is_empty() {
                    match self.app.session_system_prompt() {
                        Some(s) => {
                            let shown = if s.len() > 300 {
                                format!("{}…", &s[..300])
                            } else {
                                s.to_string()
                            };
                            self.app.push_system_message(&format!(
                                "Active system prompt override:\n\n{}\n\nUse /system clear to remove it.",
                                shown,
                            ));
                        }
                        None => self.app.push_system_message(
                            "No system prompt override set. Usage: /system <text>, or /system clear.",
                        ),
                    }
                } else {
                    self.app.set_session_system_prompt(rest.to_string());
                    let shown = if rest.len() > 80 {
                        format!("{}…", &rest[..80])
                    } else {
                        rest.to_string()
                    };
                    self.app.push_system_message(&format!(
                        "System prompt override set: {}\nPrepended to every subsequent prompt.",
                        shown,
                    ));
                }
            }
            "/summarize" => {
                if self.app.is_generating() {
                    self.app.push_system_message(
                        "Wait for the current prompt to finish before summarizing.",
                    );
                    return Ok(());
                }
                let Some(engine) = self.engine.clone() else {
                    self.app.push_system_message("Engine not initialized.");
                    return Ok(());
                };
                let transcript = self.app.export_markdown();
                if transcript.trim().is_empty() {
                    self.app.push_system_message("Nothing to summarize.");
                    return Ok(());
                }
                let prompt = format!(
                    "Summarize the following coding session in 4-6 bullet points. \
                     Capture: what was asked, what was done, any files touched, \
                     and any open questions or blockers. Be terse.\n\n{}",
                    transcript,
                );
                self.app.begin_generation();
                let event_tx = self.event_handler.sender();
                tokio::spawn(async move {
                    match engine.process(&prompt).await {
                        Ok(resp) => {
                            let text = resp.text.unwrap_or_default();
                            let _ = event_tx
                                .send(Event::ResponseComplete(text))
                                .await;
                        }
                        Err(e) => {
                            let _ = event_tx.send(Event::Error(e.to_string())).await;
                        }
                    }
                });
            }
            "/whoami" => {
                // Session summary: everything a user wants to see in
                // one place when they forget where they are.
                let model = self.app.model_label().to_string();
                let skills = self.app.loaded_skills().len();
                let msgs = self.app.messages().len();
                let ctx = self.app.context_label();
                let tools_on = self.app.show_tools();
                let auto_on = self.app.autocompact();
                let theme = self.app.theme_name().to_string();
                let goal = self.app.goal().map(|g| g.to_string());
                let sys_override = self.app.session_system_prompt().is_some();
                let attached = self.app.attached_files().len();
                let config_path = KodConfig::config_dir()
                    .ok()
                    .map(|d| d.join("config.toml").display().to_string())
                    .unwrap_or_else(|| "(unknown)".to_string());
                let session_path = crate::app::KodApp::session_path()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "(unavailable)".to_string());

                let mut msg = String::new();
                msg.push_str("Session summary\n");
                msg.push_str(&format!("  model:         {}\n", model));
                msg.push_str(&format!("  skills:        {}\n", skills));
                msg.push_str(&format!("  messages:      {}\n", msgs));
                msg.push_str(&format!("  context:       {}\n", ctx));
                msg.push_str(&format!("  theme:         {}\n", theme));
                msg.push_str(&format!("  tool output:   {}\n", if tools_on { "shown" } else { "hidden" }));
                msg.push_str(&format!("  autocompact:   {}\n", if auto_on { "on" } else { "off" }));
                msg.push_str(&format!("  system prompt: {}\n", if sys_override { "override active" } else { "(default)" }));
                msg.push_str(&format!("  attachments:   {}\n", attached));
                if let Some(g) = goal {
                    let shown: String = if g.chars().count() > 60 {
                        g.chars().take(60).collect::<String>() + "…"
                    } else {
                        g
                    };
                    msg.push_str(&format!("  goal:          {}\n", shown));
                }
                msg.push_str("\nPaths\n");
                msg.push_str(&format!("  config:  {}\n", config_path));
                msg.push_str(&format!("  session: {}\n", session_path));
                self.app.push_system_message(&msg.trim_end().to_string());
            }
            "/clearall" => {
                if self.app.is_generating() {
                    self.app.push_system_message(
                        "Wait for the current prompt to finish before clearing.",
                    );
                } else {
                    self.app.request_confirm(ConfirmKind::ClearAll);
                }
            }
            "/stats" => {
                // Per-session statistics, distinct from /context.
                let started = self.app.elapsed_session();
                let secs = started.as_secs();
                let elapsed = if secs < 60 {
                    format!("{secs}s")
                } else if secs < 3600 {
                    format!("{}m{:02}s", secs / 60, secs % 60)
                } else {
                    format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
                };

                let msgs = self.app.messages();
                let mut per_role = std::collections::HashMap::<&str, usize>::new();
                let mut tool_by_name = std::collections::HashMap::<String, usize>::new();
                for m in msgs {
                    let role = match &m.role {
                        kod_types::MessageRole::User => "you",
                        kod_types::MessageRole::Assistant => "ai",
                        kod_types::MessageRole::System => "sys",
                        kod_types::MessageRole::Tool => "tool",
                        kod_types::MessageRole::Agent(_) => "agent",
                    };
                    *per_role.entry(role).or_insert(0) += 1;
                    if let kod_types::MessageRole::Tool = m.role {
                        // The header line is `<tool_name>[ args]`, so
                        // take the first whitespace-delimited token
                        // from the stripped header.
                        let first = m.content.lines().next().unwrap_or("");
                        let header = first
                            .strip_prefix('[')
                            .and_then(|s| s.strip_suffix(']'))
                            .unwrap_or(first);
                        let tool = header.split_whitespace().next().unwrap_or("?");
                        *tool_by_name.entry(tool.to_string()).or_insert(0) += 1;
                    }
                }

                let mut msg = String::from("Session statistics\n");
                msg.push_str(&format!("  elapsed:         {}\n", elapsed));
                msg.push_str(&format!("  total messages:  {}\n", msgs.len()));
                msg.push_str(&format!("  input tokens:    {}\n", self.app.session_input_tokens()));
                msg.push_str(&format!("  output tokens:   {}\n", self.app.session_output_tokens()));
                msg.push_str(&format!("  context:         {}\n", self.app.context_label()));
                msg.push_str("\nMessages by role\n");
                for (r, n) in per_role.iter() {
                    msg.push_str(&format!("  {:<6} {}\n", r, n));
                }
                if !tool_by_name.is_empty() {
                    msg.push_str("\nTool calls this session\n");
                    let mut rows: Vec<(&String, &usize)> = tool_by_name.iter().collect();
                    rows.sort_by(|a, b| b.1.cmp(a.1));
                    for (name, n) in rows {
                        msg.push_str(&format!("  {:<20} {}\n", name, n));
                    }
                }
                self.app.push_system_message(msg.trim_end());
            }
            "/git-status" => {
                // `git status --porcelain=v2 -b` in the working
                // directory, printed as a system message.
                let cwd = std::env::current_dir()
                    .unwrap_or_else(|_| std::path::PathBuf::from("."));
                match std::process::Command::new("git")
                    .args(["status", "--porcelain=v2", "-b"])
                    .current_dir(&cwd)
                    .output()
                {
                    Ok(out) if out.status.success() => {
                        let text = String::from_utf8_lossy(&out.stdout);
                        let trimmed = text.trim_end();
                        if trimmed.is_empty() {
                            self.app.push_system_message(
                                "Working tree is clean (no changes).",
                            );
                        } else {
                            self.app.push_system_message(&format!(
                                "git status ({}):\n\n{}",
                                cwd.display(),
                                trimmed,
                            ));
                        }
                    }
                    Ok(out) => {
                        let err = String::from_utf8_lossy(&out.stderr);
                        self.app.push_system_message(&format!(
                            "git status failed: {}",
                            err.trim(),
                        ));
                    }
                    Err(e) => self.app.push_system_message(&format!(
                        "Could not run git: {e} — is git on PATH?",
                    )),
                }
            }
            "/reset" => {
                // Refuse while a generation is running: cancelling
                // state mid-turn would desync the input box from the
                // event stream.
                if self.app.is_generating() {
                    self.app.push_system_message(
                        "A generation is running — cancel it first (Esc), then /reset.",
                    );
                    return Ok(());
                }
                let cleared = self.app.reset_transient_state();
                self.app.push_system_message(&format!(
                    "Reset transient state ({} non-empty field{} cleared). Chat and memory untouched. Use /clear to wipe the chat, /clearall for everything.",
                    cleared,
                    if cleared == 1 { "" } else { "s" },
                ));
            }
            "/fork" => {
                let label = parts.next().map(|s| s.to_string());
                let n = self.app.fork_messages();
                if n == 0 {
                    self.app.push_system_message("Nothing to fork — the chat is empty.");
                } else {
                    let msg = match label {
                        Some(l) => format!(
                            "Forked {} message(s) under label {:?}. The live chat is unchanged; /undo restores this fork if the current chat is later cleared. {} fork(s) saved.",
                            n,
                            l,
                            self.app.fork_count(),
                        ),
                        None => format!(
                            "Forked {} message(s). The live chat is unchanged; /undo restores this fork if the current chat is later cleared. {} fork(s) saved.",
                            n,
                            self.app.fork_count(),
                        ),
                    };
                    self.app.push_system_message(&msg);
                }
            }
            "/check" => {
                // No argument: whole-project compiler check.
                // With a file argument: try LSP first (fast, per-file,
                // project-aware), fall back to the compiler if no
                // server is available or the file cannot be read.
                let arg = parts.next().map(|s| s.to_string());
                let cwd = std::env::current_dir()
                    .unwrap_or_else(|_| std::path::PathBuf::from("."));

                let mut handled = false;
                if let Some(file) = arg.as_deref()
                    && let Some(engine) = self.engine.clone()
                {
                    let path = if std::path::Path::new(file).is_absolute() {
                        std::path::PathBuf::from(file)
                    } else {
                        cwd.join(file)
                    };
                    if path.is_file()
                        && KodEngine::lsp_binary_for(&path).is_some()
                        && let Ok(content) = std::fs::read_to_string(&path)
                    {
                        let diags = engine
                            .lsp_diagnostics(
                                &path,
                                &content,
                                std::time::Duration::from_secs(30),
                            )
                            .await;
                        if !diags.is_empty() {
                            let mut msg = format!(
                                "LSP: {} diagnostic(s) in {}\n",
                                diags.len(),
                                path.display()
                            );
                            for d in diags.iter().take(30) {
                                let code = d
                                    .code
                                    .as_deref()
                                    .map(|c| format!("[{c}]"))
                                    .unwrap_or_default();
                                let short = if d.message.chars().count() > 120 {
                                    let s: String =
                                        d.message.chars().take(120).collect();
                                    format!("{s}…")
                                } else {
                                    d.message.clone()
                                };
                                msg.push_str(&format!(
                                    "  {} {} {}:{}:{} — {}\n",
                                    d.severity, code, d.file, d.line, d.column, short,
                                ));
                            }
                            if diags.len() > 30 {
                                msg.push_str(&format!(
                                    "  … and {} more\n",
                                    diags.len() - 30
                                ));
                            }
                            self.app.push_system_message(msg.trim_end());
                            handled = true;
                        }
                        // Empty: fall through to the compiler. An
                        // empty LSP response could mean "clean" or
                        // "LSP unreachable"; the compiler path
                        // disambiguates.
                    }
                }

                if !handled {
                    self.app.push_system_message(&format!(
                        "Running project check in {} …",
                        cwd.display()
                    ));
                    match kod_tools::CheckTool::run_check(&cwd, 120).await {
                        Ok(outcome) => {
                            if outcome.diagnostics.is_empty() {
                                self.app.push_system_message(&format!(
                                    "{}: clean ({} · exit {})",
                                    outcome.kind, outcome.command, outcome.exit_code,
                                ));
                            } else {
                                let mut msg = format!(
                                    "{}: {} diagnostic(s) ({} · exit {})\n",
                                    outcome.kind,
                                    outcome.diagnostics.len(),
                                    outcome.command,
                                    outcome.exit_code,
                                );
                                for d in outcome.diagnostics.iter().take(30) {
                                    let code = d
                                        .code
                                        .as_deref()
                                        .map(|c| format!("[{c}]"))
                                        .unwrap_or_default();
                                    let short =
                                        if d.message.chars().count() > 120 {
                                            let s: String =
                                                d.message.chars().take(120).collect();
                                            format!("{s}…")
                                        } else {
                                            d.message.clone()
                                        };
                                    msg.push_str(&format!(
                                        "  {} {} {}:{}:{} — {}\n",
                                        d.severity,
                                        code,
                                        d.file,
                                        d.line,
                                        d.column,
                                        short,
                                    ));
                                }
                                if outcome.diagnostics.len() > 30 {
                                    msg.push_str(&format!(
                                        "  … and {} more\n",
                                        outcome.diagnostics.len() - 30
                                    ));
                                }
                                if outcome.truncated {
                                    msg.push_str("(raw output truncated)\n");
                                }
                                self.app.push_system_message(msg.trim_end());
                            }
                        }
                        Err(e) => self.app.push_system_message(&format!(
                            "check failed: {e}",
                        )),
                    }
                }
            }
            "/export" => {
                let arg = parts.next().map(|s| s.to_string());
                let markdown = self.app.export_markdown();
                match arg {
                    None => {
                        self.app
                            .push_system_message(&format!(
                                "Session markdown ({} chars). To write it to a file, run /export <path>.\n\n{}",
                                markdown.len(),
                                markdown,
                            ));
                    }
                    Some(path) => {
                        let p = std::path::PathBuf::from(&path);
                        let _ = std::fs::create_dir_all(p.parent().unwrap_or(std::path::Path::new(".")));
                        match std::fs::write(&p, markdown.as_bytes()) {
                            Ok(()) => self.app.push_system_message(&format!(
                                "Exported session ({} bytes) to {}",
                                markdown.len(),
                                p.display(),
                            )),
                            Err(e) => self
                                .app
                                .push_system_message(&format!("Export failed: {e}")),
                        }
                    }
                }
            }
            "/init" => {
                let config = match KodConfig::load_default() {
                    Ok(c) => c,
                    Err(e) => {
                        self.app
                            .push_system_message(&format!("Could not load config: {e}"));
                        return Ok(());
                    }
                };
                let config_dir = KodConfig::config_dir().ok();
                let path = config_dir.as_ref().map(|d| d.join("config.toml"));
                let mut lines = vec![
                    "KOD onboarding".to_string(),
                    String::new(),
                ];
                match &path {
                    Some(p) if p.exists() => {
                        lines.push(format!("Config:   {}", p.display()));
                    }
                    Some(p) => {
                        lines.push(format!(
                            "Config:   (in memory only — could not write {})",
                            p.display()
                        ));
                    }
                    None => lines.push("Config:   (unknown — no config directory)".to_string()),
                }
                lines.push(format!("Model:    {}", config.llm.default_endpoint().model));
                lines.push(format!("Endpoint: {}", config.llm.default_endpoint().base_url));
                lines.push(format!(
                    "Network:  {}",
                    if config.llm.network_access {
                        "enabled (web_fetch can reach the network)"
                    } else {
                        "disabled (set llm.network_access = true to enable)"
                    }
                ));
                lines.push(
                    "Writes:   policy-gated (see [tools] and .kod/policy.toml)"
                        .to_string(),
                );
                lines.push(String::new());
                lines.push("Built-in model profiles:".to_string());
                for p in kod_config::profiles::PRESETS {
                    lines.push(format!("  {:<18} {}", p.name, p.description));
                    lines.push(format!("    model:    {}", p.model));
                    if let Some(cmd) = p.install_command {
                        lines.push(format!("    install:  {}", cmd));
                    }
                }
                lines.push(String::new());
                lines.push("Switch profiles with:  kod profile use <name>".to_string());
                lines.push(String::new());
                lines.push("Next steps:".to_string());
                lines.push("  1. Start the model server (e.g. `ollama serve`)".to_string());
                lines.push(format!(
                    "  2. Pull the model (e.g. `ollama pull {}`)",
                    config.llm.default_endpoint().model
                ));
                lines.push("  3. Verify the setup:  kod doctor (or /doctor)".to_string());
                lines.push("  4. Read skills: /skills".to_string());
                self.app.push_system_message(&lines.join("\n"));
            }
            "/doctor" => {
                let config = match KodConfig::load_default() {
                    Ok(c) => c,
                    Err(e) => {
                        self.app
                            .push_system_message(&format!("Could not load config: {e}"));
                        return Ok(());
                    }
                };
                let report = kod_core::doctor::run_diagnostics(&config);
                let mut lines = vec!["KOD doctor".to_string(), String::new()];
                for check in &report.checks {
                    let mark = match check.status {
                        kod_core::doctor::CheckStatus::Ok => "✓",
                        kod_core::doctor::CheckStatus::Warn => "⚠",
                        kod_core::doctor::CheckStatus::Fail => "✗",
                    };
                    lines.push(format!("  {} {:<14} {}", mark, check.name, check.message));
                }
                lines.push(String::new());
                if report.has_failures() {
                    lines.push(
                        "One or more checks failed — review the items marked ✗ above."
                            .to_string(),
                    );
                } else {
                    lines.push("All checks passed.".to_string());
                }
                self.app.push_system_message(&lines.join("\n"));
            }
            _ => {
                // User-defined commands from `[commands]` in the config.
                // A `/foo` that is not a builtin is looked up by name;
                // a match expands `{args}` and `{cwd}` and dispatches
                // as a normal prompt.
                let config = KodConfig::load_default().ok();
                let custom = config.as_ref().and_then(|c| {
                    let key = cmd.trim_start_matches('/');
                    c.commands.get(key).cloned()
                });
                match custom {
                    Some(body) => {
                        let args: String =
                            parts.collect::<Vec<_>>().join(" ");
                        let cwd = std::env::current_dir()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|_| ".".to_string());
                        let expanded = body
                            .replace("{args}", &args)
                            .replace("{cwd}", &cwd);
                        self.app.set_input(expanded);
                        Box::pin(self.dispatch_prompt()).await?;
                    }
                    None => {
                        // Helpful hint when the user typed something
                        // close to a custom command's name.
                        let hint = config
                            .as_ref()
                            .map(|c| {
                                let names: Vec<&str> = c
                                    .commands
                                    .keys()
                                    .map(|s| s.as_str())
                                    .collect();
                                if names.is_empty() {
                                    String::new()
                                } else {
                                    format!(
                                        " Custom commands available: {}",
                                        names
                                            .iter()
                                            .map(|n| format!("/{n}"))
                                            .collect::<Vec<_>>()
                                            .join(", "),
                                    )
                                }
                            })
                            .unwrap_or_default();
                        self.app.push_system_message(&format!(
                            "Unknown command: {} — try /help.{}",
                            cmd, hint,
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    /// Swap the engine's provider to another model on the same endpoint.
    ///
    /// Warns (does not block) when the name is not in the list the
    /// provider last reported. The switch always succeeds — the
    /// provider constructor never fails on a name it does not
    /// recognize, and the list can be stale (a model pulled after the
    /// TUI started). But the previous behavior always printed
    /// "Switched model to X" as if it had succeeded, and the first
    /// prompt afterwards failed with an opaque "model not found".
    /// Naming the mismatch at switch time lets the user correct it
    /// before wasting a prompt.
    async fn switch_model(&mut self, name: &str) -> Result<()> {
        let Some(engine) = self.engine.clone() else {
            self.app.push_system_message("Engine not initialized");
            return Ok(());
        };
        // available_models() is empty before the first list_models
        // call or when list_models failed — treat that as "unknown,
        // do not warn" rather than "no model is valid".
        let available = self.app.available_models();
        let unknown = !available.is_empty() && !available.iter().any(|m| m == name);

        // The registry is the source of truth: a model switch is a
        // `ModelRef` change on the current endpoint, not a provider
        // rebuild. The endpoint name comes from the current ModelRef.
        let current = engine.current_model().await;
        engine
            .set_current_model(kod_provider::ModelRef::new(current.endpoint.clone(), name))
            .await;
        self.app.set_model_name(name);
        if unknown {
            self.app.push_system_message(&format!(
                "Switched to '{}' — not in the server's last model list. \
                 If the next prompt fails, run `/model` (no args) to see what the \
                 server has, or `ollama pull {}` to fetch it.",
                name, name
            ));
        } else {
            self.app
                .push_system_message(&format!("Switched model to {}", name));
        }
        Ok(())
    }

    /// Fetch the current model list from the engine's provider, refresh
    /// `KodApp::available_models` (so the next `/model` completion is
    /// accurate), and print the list to chat.
    ///
    /// Calling this before `switch_model` is the natural recovery when
    /// a user has pulled a new model with `ollama pull` after the TUI
    /// started: the cached list is stale and `/model <new>` would warn
    /// spuriously.
    async fn show_and_refresh_models(&mut self) -> Result<()> {
        let Some(engine) = &self.engine else {
            self.app.push_system_message("Engine not initialized");
            return Ok(());
        };
        let models = match engine.list_models().await {
            Ok(m) => m,
            Err(e) => {
                // The provider is set but the request failed. Name the
                // failure instead of reporting an empty list — the
                // user's recovery step differs: fix the server, not
                // "there is nothing to see."
                self.app.push_system_message(&format!(
                    "Could not list models from the provider: {e}\n\
                     Check that the server is running and `base_url` in the \
                     kod config is correct. For Ollama: `ollama serve`, then \
                     `/model` again.",
                ));
                return Ok(());
            }
        };
        if models.is_empty() {
            // No error, but the server really has zero models.
            self.app.push_system_message(
                "The provider is reachable but reports no models. \
                 Pull one first (e.g. `ollama pull qwen2.5:0.5b`), then \
                 `/model` again.",
            );
            return Ok(());
        }
        self.app.set_available_models(models.clone());
        let current = self.app.model_name().to_string();
        let mut lines: Vec<String> = models
            .iter()
            .map(|m| {
                if m == &current {
                    format!("- {m}  (current)")
                } else {
                    format!("- {m}")
                }
            })
            .collect();
        lines.sort();
        self.app.push_system_message(&format!(
            "Models ({}):\n{}\n\nSwitch with: /model <name>",
            models.len(),
            lines.join("\n")
        ));
        Ok(())
    }

    /// Handle key events
    async fn handle_key(&mut self, key: KeyCode) -> Result<()> {
        // Question dialog: while it is up, all printable characters
        // go into the answer buffer, Backspace edits, Enter submits,
        // Esc/Ctrl+C cancels.
        if self.app.is_asking() {
            match key {
                KeyCode::Char(c) => {
                    self.app.question_input_mut().push(c);
                }
                KeyCode::Backspace => {
                    self.app.question_input_mut().pop();
                }
                KeyCode::Enter => {
                    if let Some(q) = self.app.pending_question() {
                        let id = q.id;
                        let answer = self.app.clear_pending_question();
                        if let Some(engine) = &self.engine {
                            engine.respond_to_question(id, answer).await;
                        }
                    }
                }
                KeyCode::Escape | KeyCode::CtrlC => {
                    if let Some(q) = self.app.pending_question() {
                        let id = q.id;
                        self.app.clear_pending_question();
                        if let Some(engine) = &self.engine {
                            engine
                                .respond_to_question(id, "(cancelled)".to_string())
                                .await;
                        }
                    }
                }
                _ => {}
            }
            return Ok(());
        }

        // Approval dialog: while it is up, y / n / a / Esc / Ctrl+C
        // answer the request; every other key is swallowed so the
        // user does not type past a modal they cannot dismiss.
        //
        //   y -> Approve (this call runs)
        //   n -> Deny (this call is refused; the next matching call
        //        prompts again)
        //   a -> DenyAlways (this call is refused AND a session deny
        //        rule is registered so the same tool / path pattern
        //        does not prompt again for the rest of the process)
        //   Esc / Ctrl+C -> Deny (same as n)
        if self.app.is_approving() {
            let decision = match key {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    Some(kod_core::engine::ApprovalDecision::Approve)
                }
                KeyCode::Char('n') | KeyCode::Char('N') => {
                    Some(kod_core::engine::ApprovalDecision::Deny)
                }
                KeyCode::Char('a') | KeyCode::Char('A') => {
                    Some(kod_core::engine::ApprovalDecision::DenyAlways)
                }
                KeyCode::Escape | KeyCode::CtrlC => {
                    Some(kod_core::engine::ApprovalDecision::Deny)
                }
                _ => None,
            };
            if let Some(decision) = decision {
                if let Some(approval) = self.app.pending_approval() {
                    let id = approval.id;
                    self.app.clear_pending_approval();
                    if let Some(engine) = &self.engine {
                        engine.respond_to_approval(id, decision).await;
                    }
                }
            }
            return Ok(());
        }

        // Type-ahead search: while the bar is open and being edited,
        // every printable character, Backspace, and Escape belongs to
        // the query, not the input box. This check runs before the
        // yes/no intercept below so a search started while a confirm
        // is pending still types into the query — the user can see
        // what they are typing.
        if self.app.is_editing_search() {
            match key {
                KeyCode::Char(c) => {
                    self.app.search_type(c);
                    return Ok(());
                }
                KeyCode::Backspace => {
                    self.app.search_backspace();
                    return Ok(());
                }
                KeyCode::Escape | KeyCode::CtrlC => {
                    self.app.clear_search();
                    self.app.push_system_message("Search cleared.");
                    return Ok(());
                }
                KeyCode::Enter => {
                    // Commit: leave the query in place, drop out of
                    // editing so n/N navigate. If the query is empty,
                    // there is nothing to commit — clear instead.
                    if self.app.search_query_text().is_empty() {
                        self.app.clear_search();
                    } else {
                        self.app.commit_search();
                    }
                    return Ok(());
                }
                _ => {
                    // Other keys fall through to normal handling so
                    // the user can still scroll (PgUp/PgDn), quit, etc.
                }
            }
        }

        // Intercept yes/no when a destructive action is pending (q, /clear).
        if self.app.pending_confirm().is_some() {
            match key {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    let kind = self.app.resolve_confirm(true);
                    match kind {
                        Some(ConfirmKind::Clear) => {
                            if let Some(engine) = &self.engine {
                                engine.clear_history().await;
                            }
                        }
                        Some(ConfirmKind::ClearAll) => {
                            // Clear chat (already done by resolve),
                            // engine history, long-term memory, and
                            // checkpoints for this project.
                            if let Some(engine) = &self.engine {
                                engine.clear_history().await;
                            }
                            if let Ok(config) = KodConfig::load_default()
                                && let Ok(path) = config.memory_db_path()
                                && let Ok(manager) = kod_memory::MemoryManager::new(
                                    path,
                                    config.memory.short_term_capacity,
                                )
                                && let Ok(all) = manager.get_all_long_term().await
                            {
                                let n = all.len();
                                for e in &all {
                                    let _ = manager
                                        .remove(
                                            kod_types::MemoryType::LongTerm,
                                            &e.id,
                                        )
                                        .await;
                                }
                                self.app.push_system_message(&format!(
                                    "Cleared {} long-term memory entr{}.",
                                    n,
                                    if n == 1 { "y" } else { "ies" },
                                ));
                            }
                            if let Some(engine) = &self.engine
                                && let Some(cp) = engine.checkpoints()
                                && let Ok(n) = cp.clear()
                            {
                                self.app.push_system_message(&format!(
                                    "Cleared {} checkpoint(s).",
                                    n,
                                ));
                            }
                            self.app.push_system_message(
                                "Cleared all: chat, engine history, long-term memory, checkpoints.",
                            );
                        }
                        _ => {}
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

    /// Handle keys in normal mode.
    ///
    /// Single-character keys are looked up in the user's binding map
    /// (loaded from `~/.config/kod/tui_keys.toml` or a project-local
    /// `.kod-keys.toml`; see [`crate::keybindings`]). A bound character
    /// dispatches through [`TuiLoop::dispatch_key_action`]. Unbound
    /// characters fall through to the small set of fixed controls:
    /// `r` retry, `o` expand the newest tool row, `n`/`N` step through
    /// chat-search matches. Non-character keys (Escape, Ctrl+C, Tab,
    /// F1, arrows, Home/End, PgUp/PgDn) keep their fixed behaviour.
    async fn handle_normal_mode_key(&mut self, key: KeyCode) -> Result<()> {
        if let KeyCode::Char(c) = key
            && let Some(action) = self.keybindings.get(&c).copied()
        {
            return self.dispatch_key_action(action).await;
        }
        match key {
            // Fixed single-char fallbacks not exposed through the
            // configurable binding set.
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
            KeyCode::F(1) => {
                self.app.toggle_help();
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

    /// Dispatch one configurable keybinding action.
    ///
    /// The behaviour bodies are identical to what the old hardcoded
    /// match arms did; extracting them lets the binding table and the
    /// code path share one implementation.
    async fn dispatch_key_action(&mut self, action: KeyAction) -> Result<()> {
        match action {
            KeyAction::Insert => self.app.set_input_mode(InputMode::Insert),
            KeyAction::Quit => {
                if self.app.is_generating() {
                    self.app.request_confirm(ConfirmKind::Quit);
                } else {
                    self.app.quit();
                }
            }
            KeyAction::Help => self.app.toggle_help(),
            KeyAction::Panel => self.app.set_mode(AppMode::AgentPanel),
            KeyAction::ScrollUp => self.app.scroll_up(1),
            KeyAction::ScrollDown => self.app.scroll_down(1),
            KeyAction::Top => self.app.scroll_to_top(),
            KeyAction::Bottom => self.app.scroll_to_bottom(),
            KeyAction::EditLast => {
                self.app.edit_last_message();
                self.app.set_input_mode(InputMode::Insert);
            }
            KeyAction::Undo => {
                if self.app.undo_clear() {
                    self.app
                        .push_system_message("Restored last cleared messages.");
                } else {
                    self.app.push_system_message("Nothing to undo.");
                }
            }
            KeyAction::ToggleTools => {
                let on = self.app.toggle_show_tools();
                self.app.push_system_message(if on {
                    "Tool outputs shown."
                } else {
                    "Tool outputs hidden."
                });
            }
            KeyAction::SearchPrefix => {
                // Same as `/search` with no argument: open the bar
                // and let the user's next keystroke be the first
                // character of the query.
                self.app.begin_search();
            }
            KeyAction::CopyLast => {
                if self.app.copy_last_to_clipboard() {
                    self.app
                        .push_system_message("Copied last assistant reply to clipboard.");
                } else {
                    self.app
                        .push_system_message("Nothing to copy — no assistant reply yet.");
                }
            }
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
                // Uses the in-place delete method so the cursor stays
                // where it was. The previous implementation called
                // `set_input`, which resets `cursor_position` to the end
                // of the input — a visible jump on every Delete press.
                self.app.delete_at_cursor();
                self.app.reset_completion();
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

    /// Every entry in `SLASH_COMMANDS` must appear in `SLASH_HELP`, so
    /// adding a command to the autocomplete without documenting it
    /// fails this test. The previous SLASH_HELP was missing `/debug`
    /// for several commits — this pins the invariant.
    #[test]
    fn test_slash_help_lists_every_command() {
        use crate::app::SLASH_COMMANDS;
        let help = SLASH_HELP;
        for cmd in SLASH_COMMANDS {
            assert!(
                help.contains(cmd.name),
                "SLASH_COMMANDS entry {:?} is not mentioned in SLASH_HELP",
                cmd.name
            );
        }
        // Every `/`-leading token inside SLASH_HELP should also be a
        // known command, so a typo'd name does not linger. Split on
        // whitespace, keep tokens starting with '/', strip trailing
        // punctuation from each. Compare against the SLASH_COMMANDS set.
        let known: std::collections::HashSet<&'static str> =
            SLASH_COMMANDS.iter().map(|c| c.name).collect();
        for token in help.split_whitespace() {
            let trimmed = token.trim_end_matches(|c: char| {
                !c.is_ascii_alphanumeric() && c != '/' && c != '-'
            });
            if trimmed.starts_with('/') && trimmed.len() > 1 {
                assert!(
                    known.contains(trimmed),
                    "SLASH_HELP mentions {:?} which is not in SLASH_COMMANDS",
                    trimmed
                );
            }
        }
    }

    /// The idle hint line must name `f` as the search key, matching the
    /// default keybinding, and must not claim `/` starts a search.
    #[tokio::test]
    async fn test_idle_hint_names_search_key_correctly() {
        let tui = TuiLoop::new();
        let hint = tui.app().hint_line();
        assert!(
            hint.contains("f search"),
            "hint should name the f key for search: {hint}"
        );
        assert!(
            !hint.contains("/ search"),
            "hint should not claim / starts a search: {hint}"
        );
    }

    /// `/theme light` must actually change the palette and report the
    /// transition.
    #[tokio::test]
    async fn test_theme_known_name_applies() {
        let mut tui = TuiLoop::new();
        tui.handle_command("/theme light").await.unwrap();
        assert_eq!(tui.app().theme_name(), "light");
        let last = tui.app().messages().last().unwrap();
        assert!(last.content.contains("→ light"), "got: {}", last.content);
    }

    /// `/theme neon` must not claim to have switched to a theme that
    /// does not exist. Regression: the previous handler printed
    /// "Theme dark → neon" while the palette fell back to dark.
    #[tokio::test]
    async fn test_theme_unknown_name_reports_fallback() {
        let mut tui = TuiLoop::new();
        tui.handle_command("/theme neon").await.unwrap();
        // Palette fell back to dark.
        assert_eq!(tui.app().theme_name(), "dark");
        let last = tui.app().messages().last().unwrap();
        assert!(
            last.content.contains("Unknown theme"),
            "expected an 'Unknown theme' message, got: {}",
            last.content
        );
        assert!(
            last.content.contains("dark, light"),
            "should name the known themes: {}",
            last.content
        );
        assert!(
            !last.content.contains("→ neon"),
            "must not lie about switching to 'neon': {}",
            last.content
        );
    }

    /// Delete key (insert mode) must remove the character under the
    /// cursor and leave the cursor where it was. Regression: the
    /// previous implementation routed through `set_input`, which snaps
    /// `cursor_position` to the end of the input.
    #[tokio::test]
    async fn test_delete_preserves_cursor_position() {
        let mut tui = TuiLoop::new();
        tui.app_mut().set_input_mode(InputMode::Insert);
        tui.app_mut().set_input("hello world".to_string());
        // Cursor is at end after set_input; move left to sit on 'w'.
        for _ in 0..5 {
            tui.handle_event(Event::Key(KeyCode::Left)).await.unwrap();
        }
        assert_eq!(tui.app().cursor_position(), 6);

        tui.handle_event(Event::Key(KeyCode::Delete)).await.unwrap();
        assert_eq!(tui.app().input(), "hello orld");
        assert_eq!(
            tui.app().cursor_position(),
            6,
            "cursor must stay put after Delete, not jump to end"
        );
    }

    /// Backspace still removes the character before the cursor and
    /// moves the cursor left by one — pin against future changes.
    #[tokio::test]
    async fn test_backspace_still_moves_cursor_left() {
        let mut tui = TuiLoop::new();
        tui.app_mut().set_input_mode(InputMode::Insert);
        tui.app_mut().set_input("hello".to_string());
        tui.handle_event(Event::Key(KeyCode::Backspace)).await.unwrap();
        assert_eq!(tui.app().input(), "hell");
        assert_eq!(tui.app().cursor_position(), 4);
    }

    /// The default binding set must keep 'i' as insert, so a fresh
    /// install behaves as documented.
    #[tokio::test]
    async fn test_default_keybinding_insert_fires() {
        let mut tui = TuiLoop::new();
        tui.handle_event(Event::Key(KeyCode::Char('i')))
            .await
            .unwrap();
        assert_eq!(tui.app().input_mode(), &InputMode::Insert);
    }

    /// A user-supplied binding for a previously-unbound key must fire.
    #[tokio::test]
    async fn test_custom_keybinding_is_honored() {
        use crate::keybindings::KeyAction;
        use std::collections::HashMap;

        let mut tui = TuiLoop::new();
        let mut b = HashMap::new();
        b.insert('w', KeyAction::Insert);
        tui.set_keybindings(b);

        // 'w' is not in the defaults; only the custom binding should
        // make it enter insert mode.
        tui.handle_event(Event::Key(KeyCode::Char('w')))
            .await
            .unwrap();
        assert_eq!(tui.app().input_mode(), &InputMode::Insert);
    }

    /// Replacing the default binding for an action with a different key
    /// must disable the old key. Regression: the previous implementation
    /// hardcoded the character arms, so a rebind would silently keep the
    /// old key working and the new key would never fire.
    #[tokio::test]
    async fn test_rebinding_replaces_default() {
        use crate::keybindings::KeyAction;
        use std::collections::HashMap;

        let mut tui = TuiLoop::new();
        let mut b = HashMap::new();
        // Map insert to 'w' only — 'i' must no longer trigger it.
        b.insert('w', KeyAction::Insert);
        tui.set_keybindings(b);

        tui.handle_event(Event::Key(KeyCode::Char('i')))
            .await
            .unwrap();
        assert_eq!(
            tui.app().input_mode(),
            &InputMode::Normal,
            "rebound 'i' should no longer enter insert mode"
        );

        tui.handle_event(Event::Key(KeyCode::Char('w')))
            .await
            .unwrap();
        assert_eq!(tui.app().input_mode(), &InputMode::Insert);
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

    /// `/search` with no argument opens the type-ahead search bar: the
    /// app enters the editing state with an empty query, and the next
    /// keystroke is a character of the query (see `handle_key`). This
    /// replaces the earlier workaround that prefilled the input box
    /// with "/search " — the bar is the design the command was meant
    /// to have.
    #[tokio::test]
    async fn test_search_no_arg_opens_type_ahead_bar() {
        let mut tui = TuiLoop::new();
        tui.handle_command("/search").await.unwrap();

        assert!(
            tui.app().is_editing_search(),
            "no-arg /search must open the search bar"
        );
        assert_eq!(tui.app().search_query_text(), "");
        // The input box is untouched — the user's next keystroke goes
        // into the query, not the input.
        assert_eq!(tui.app().input(), "");

        // Type a query and confirm it landed in the search, not the
        // input.
        tui.handle_event(Event::Key(KeyCode::Char('x')))
            .await
            .unwrap();
        assert_eq!(tui.app().search_query_text(), "x");
        assert_eq!(tui.app().input(), "");
    }

    /// `/search <text>` still works as before: finds matches and
    /// reports the count.
    #[tokio::test]
    async fn test_search_with_arg_still_works() {
        let mut tui = TuiLoop::new();
        for i in 0..3 {
            tui.app_mut().push_system_message(&format!("needle {i}"));
        }
        tui.handle_command("/search needle").await.unwrap();
        let last = tui.app().messages().last().unwrap();
        assert!(
            last.content.contains("3 match"),
            "expected 3 matches: {}",
            last.content
        );
    }

    /// `/swarm` with no argument prints the usage line — it must not
    /// fall through to `dispatch_swarm`, which would fail trying to
    /// spawn a runner for an empty goal.
    #[tokio::test]
    async fn test_swarm_command_no_arg_shows_usage() {
        let mut tui = TuiLoop::new();
        tui.handle_command("/swarm").await.unwrap();
        let last = tui.app().messages().last().unwrap();
        assert!(
            last.content.contains("Usage: /swarm"),
            "expected usage line, got: {}",
            last.content
        );
        // No run started.
        assert!(!tui.app().is_generating());
    }

    /// `/swarm <goal>` without an engine reports that rather than
    /// silently doing nothing. This is the routing test: it proves the
    /// `/swarm` arm in `handle_command` reaches `dispatch_swarm`, which
    /// is where the "no engine" check lives.
    #[tokio::test]
    async fn test_swarm_command_without_engine_reports() {
        let mut tui = TuiLoop::new();
        // TuiLoop::new() has no engine; that is the case this asserts.
        tui.handle_command("/swarm fix the payment handler").await.unwrap();
        let last = tui.app().messages().last().unwrap();
        assert!(
            last.content.contains("Engine not initialized"),
            "expected engine-missing message, got: {}",
            last.content
        );
    }

    /// Dispatching a plain prompt must record it as `last_prompt`, so
    /// `/retry` and the `r` key have something to resend. Regression:
    /// before this, last_prompt was only set by retry_generation
    /// itself — a chicken-and-egg that made /retry a no-op.
    #[tokio::test]
    async fn test_retry_prompt_is_recorded_on_dispatch() {
        let mut tui = TuiLoop::new();
        tui.app_mut().set_input_mode(InputMode::Insert);
        tui.app_mut().set_input("hello there".to_string());
        // dispatch_prompt with no engine records the message and returns.
        tui.handle_event(Event::Key(KeyCode::Enter)).await.unwrap();
        assert_eq!(
            tui.app().last_prompt(),
            Some("hello there"),
            "last_prompt must be set by dispatch_prompt"
        );
    }

    /// Slash commands must not become the retry target: /retry should
    /// resend a user prompt, not re-run a /command.
    #[tokio::test]
    async fn test_slash_commands_do_not_become_retry_target() {
        let mut tui = TuiLoop::new();
        tui.app_mut().set_input_mode(InputMode::Insert);
        tui.app_mut().set_input("real prompt".to_string());
        tui.handle_event(Event::Key(KeyCode::Enter)).await.unwrap();
        assert_eq!(tui.app().last_prompt(), Some("real prompt"));

        tui.app_mut().set_input("/help".to_string());
        tui.handle_event(Event::Key(KeyCode::Enter)).await.unwrap();
        assert_eq!(
            tui.app().last_prompt(),
            Some("real prompt"),
            "/help should not become the retry target"
        );
    }

    /// `/model` with no argument must route to show_and_refresh_models.
    /// Without an engine the handler reports that clearly rather than
    /// printing the old "Usage: /model <name>".
    #[tokio::test]
    async fn test_model_with_no_args_lists_or_reports_engine_missing() {
        let mut tui = TuiLoop::new();
        tui.app_mut()
            .set_available_models(vec!["qwen2.5:0.5b".to_string()]);
        tui.handle_command("/model").await.unwrap();
        let last = tui.app().messages().last().unwrap();
        assert!(
            last.content.contains("Engine not initialized"),
            "got: {}",
            last.content
        );
    }

    /// available_models() reports what was last set. Pins the shape
    /// switch_model relies on: an empty list means 'unknown' (do not
    /// warn), a non-empty list is what validation compares against.
    #[tokio::test]
    async fn test_available_models_accessor_roundtrips() {
        let mut tui = TuiLoop::new();
        assert!(tui.app().available_models().is_empty());
        tui.app_mut()
            .set_available_models(vec!["a".to_string(), "b".to_string()]);
        let got = tui.app().available_models();
        assert_eq!(got.len(), 2);
        assert!(got.contains(&"a".to_string()));
        assert!(got.contains(&"b".to_string()));
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
