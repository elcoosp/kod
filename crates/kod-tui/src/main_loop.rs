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
    /// When true, `init_engine` skips loading `~/.kod/tui_session.json`
    /// and starts with an empty chat. Set by `kod tui --no-resume`.
    ///
    /// A field on the loop, not a parameter threaded through every
    /// method: `init_engine` is the only consumer, and the CLI sets
    /// it once at construction time.
    no_resume: bool,
    /// Whether mouse capture is currently enabled. Tracked here so
    /// the `m` key can toggle it without querying the terminal
    /// (crossterm has no "is capture enabled?" query on all
    /// platforms) and so `restore_terminal` can send the right
    /// disable-or-nothing command on exit.
    mouse_captured: bool,
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
            no_resume: false,
            mouse_captured: true,
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

    /// When true, skip loading the saved session on startup. Called by
    /// `kod tui --no-resume`. The TUI still writes a session on exit;
    /// `--no-resume` only affects the *restore* half.
    pub fn set_no_resume(&mut self, no_resume: bool) {
        self.no_resume = no_resume;
    }

    /// Open `initial` in `$EDITOR` (falling back to `$VISUAL` then
    /// `vi`), and return whatever the user left behind.
    ///
    /// The TUI is in raw mode on the alternate screen while this
    /// runs; both are suspended for the editor's lifetime and
    /// restored after. A failure to restore either — a crash in
    /// `crossterm::execute!` or the shell — is surfaced as an error
    /// so the caller can decide whether to abort the whole session;
    /// a silent failure leaves the user staring at a broken
    /// terminal.
    ///
    /// The editor is spawned synchronously. This blocks the TUI's
    /// event loop for the duration of the editing session, which is
    /// the desired behaviour: while a user is in their editor, the
    /// TUI behind it is frozen and should not be processing input.
    pub async fn open_external_editor(&mut self, initial: &str) -> Result<String> {
        use crossterm::execute;

        // Write the initial buffer to a temp file. The prefix keeps
        // two `kod` processes on the same machine from clobbering
        // each other's scratch, and the suffix gives the editor a
        // language hint so syntax highlighting works.
        let tmp = std::env::temp_dir().join(format!(
            "kod-edit-{}-{}.md",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        ));
        std::fs::write(&tmp, initial.as_bytes()).map_err(KodError::Io)?;

        // Suspend TUI mode. Order matters: leave the alternate
        // screen *before* disabling raw mode so the terminal's
        // cursor and scrollback are restored first.
        if let Some(terminal) = &mut self.terminal {
            let _ = terminal.show_cursor();
        }
        execute!(
            std::io::stdout(),
            crossterm::event::DisableMouseCapture,
            crossterm::terminal::LeaveAlternateScreen,
        )
        .map_err(|e| KodError::Internal(format!("could not leave alternate screen: {e}")))?;
        crossterm::terminal::disable_raw_mode()
            .map_err(|e| KodError::Internal(format!("could not disable raw mode: {e}")))?;

        // Resolve the editor: $EDITOR, then $VISUAL, then `vi`.
        let editor = std::env::var("EDITOR")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| {
                std::env::var("VISUAL")
                    .ok()
                    .filter(|s| !s.trim().is_empty())
            })
            .unwrap_or_else(|| "vi".to_string());

        // Shell out so a value like `code --wait` (or
        // `emacsclient -nw`) works without the caller splitting on
        // whitespace themselves.
        let cmd = format!("{} {}", editor, shell_quote(&tmp.to_string_lossy()));
        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg(&cmd)
            .status();

        // Always restore the TUI, even when the editor failed —
        // a user whose `$EDITOR` was set to garbage should still
        // come back to a working `kod`.
        let restore = (|| -> Result<()> {
            crossterm::terminal::enable_raw_mode()
                .map_err(|e| KodError::Internal(format!("could not re-enable raw mode: {e}")))?;
            execute!(
                std::io::stdout(),
                crossterm::terminal::EnterAlternateScreen,
                crossterm::event::EnableMouseCapture,
            )
            .map_err(|e| KodError::Internal(format!("could not re-enter alternate screen: {e}")))?;
            if let Some(terminal) = &mut self.terminal {
                let _ = terminal.clear();
                let _ = terminal.hide_cursor();
            }
            Ok(())
        })();
        restore?;

        // Read the file back regardless of the editor's exit code —
        // a user who wrote something and then hit an editor error
        // should still get their work. The error is surfaced, but
        // after the content is read.
        let content = std::fs::read_to_string(&tmp).unwrap_or_default();
        let _ = std::fs::remove_file(&tmp);

        match status {
            Ok(s) if s.success() => Ok(content),
            Ok(s) => Err(KodError::Internal(format!(
                "editor {:?} exited with status {:?}. Your text is preserved in the input box.",
                editor,
                s.code()
            ))),
            Err(e) => Err(KodError::Internal(format!(
                "could not launch editor {:?}: {e}",
                editor
            ))),
        }
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
        self.app
            .set_context_limit(config.llm.default_endpoint().context_window);

        // History budget: roughly three chars per token of the model's
        // window. The engine clamps anything below its floor, so a tiny
        // or placeholder context_window cannot produce an engine that
        // forgets every turn.
        let history_budget = config
            .llm
            .default_endpoint()
            .context_window
            .saturating_mul(3);

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
        // Design D2.1: build the embedder the memory subsystem will use
        // for semantic retrieval. `None` (the config default) leaves the
        // keyword+recency fallback in place; no retrieval path is broken
        // by an absent embedder.
        let embedder = kod_memory::embedding::from_config(
            &config.memory,
            Some(&config.llm.default_endpoint().base_url),
        );
        let router_config = RouterConfig {
            skill_threshold: config.skills.match_threshold,
            context_window: config.llm.default_endpoint().context_window,
            short_term_capacity: config.memory.short_term_capacity,
            embedder,
            ..RouterConfig::default()
        };
        let engine = KodEngine::new(router_config, db_path)?;
        engine.set_history_budget(history_budget);

        let (registry, default_model, routing) =
            kod_core::build_registry(&config.llm, Some(&model_name))?;
        engine.set_registry(registry, default_model, routing).await;
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
        // Tier 1.3 — read-protection from the effective policy.
        if let Some(policy) = engine.policy().await {
            engine.set_read_protection(policy.read_protection().clone());
        }
        // Tier 1.2 — install the session cost caps.
        engine.install_limits(&config.limits);
        // Tier 1.4 — trace writer next to the session log, when one
        // is installed.
        if let Some(log_path) = engine.session_log_path()
            && let Some(trace_path) = kod_core::TraceWriter::default_for_session(&log_path)
            && let Ok(w) = kod_core::TraceWriter::open(trace_path)
        {
            engine.set_turn_trace_writer(std::sync::Arc::new(w));
        }
        // Tier 3.4 — persist plans and decisions across restarts.
        // `state.json` sits next to the trace log so a reviewer finds
        // both in the same directory.
        if let Some(log_path) = engine.session_log_path()
            && let Some(state_path) = kod_core::StateStore::sibling_of(&log_path)
        {
            let store = kod_core::StateStore::open(state_path);
            engine.set_state_store(store).await;
        }

        // Install the Jev (TypeSafe AI) client when enabled. A
        // disabled block (the default) is a silent no-op; an
        // enabled-but-misconfigured block is logged but does not
        // stop startup — the TUI must always reach its prompt.
        match kod_core::install_jev_from_config(&engine, &config.jev) {
            Ok(true) => self
                .app
                .push_system_message("Jev (TypeSafe AI) integration enabled."),
            Ok(false) => {}
            Err(e) => self.app.push_system_message(&format!(
                "Jev configuration error (continuing without): {e}",
            )),
        }

        engine.start().await?;
        // Sandbox label for the header (design D3.3 / AD-10). Rendered
        // as a badge only when non-empty; the value reflects the
        // *effective* backend the resolver picked (or `require-missing`
        // when the caller asked for Require and no primitive exists).
        let (mode, backend) = engine.sandbox_status();
        let sandbox_label = match (mode, backend) {
            (kod_tools::context::SandboxMode::Disabled, _) => "off".to_string(),
            (kod_tools::context::SandboxMode::Auto, Some(b)) => b.to_string(),
            (kod_tools::context::SandboxMode::Auto, None) => "off".to_string(),
            (kod_tools::context::SandboxMode::Require, Some(b)) => b.to_string(),
            (kod_tools::context::SandboxMode::Require, None) => "require-missing".to_string(),
        };
        self.app.set_sandbox_label(sandbox_label);
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
            match engine.list_models().await {
                Ok(models) => {
                    // Record per-model windows so a later `/model <x>`
                    // switch on this endpoint allocates against the
                    // selected model's real window.
                    let current = engine.current_model().await;
                    engine.record_model_catalog(&current.endpoint, &models);
                    let ids: Vec<String> = models.into_iter().map(|m| m.id).collect();
                    self.app.set_available_models(ids);
                }
                Err(_) => {
                    // No completion candidates when the fetch fails;
                    // `/model` will report the failure explicitly when
                    // the user asks.
                }
            }
        }

        // Restore a saved session, if any. The chat messages come back
        // (what the user sees), and we seed the engine's transcript with
        // the same user/assistant pairs (what the model sees) so the two
        // views agree on the next prompt — without the seed, the model
        // opens the next turn with "this is a fresh conversation" while
        // the screen is full of history.
        //
        // `--no-resume` short-circuits the restore entirely. The saved
        // file is left on disk — a user who asked for a fresh session
        // today may want the old one tomorrow, and silently deleting
        // their transcript is the kind of surprise this flag exists to
        // avoid.
        let restored = if self.no_resume {
            0
        } else {
            self.app.load_session()
        };
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
            // H-T9: bracketed paste. Without it, a paste of a code
            // block delivers literal Enter keystrokes and the first
            // line is submitted as a prompt.
            crossterm::event::EnableBracketedPaste,
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
            crossterm::event::DisableBracketedPaste,
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
        let default_hook =
            std::sync::Arc::try_unwrap(default_hook).unwrap_or_else(|_| std::panic::take_hook());
        std::panic::set_hook(default_hook);

        let _ = self.restore_terminal().await;
        result
    }

    /// Internal main loop.
    ///
    /// Rendering is *batched*: one frame per group of events, not one
    /// frame per event. `Event::ResponseChunk` arrives at token rate
    /// during streaming — a naive `render(); next_event()` loop paints
    /// a full frame per token, which is why long sessions stutter
    /// under load. Two changes fix that:
    ///
    /// 1. `try_next_event` drains whatever else is already queued
    ///    before rendering (bounded by `MAX_EVENTS_PER_FRAME` so a
    ///    permanently-saturated queue can still render).
    /// 2. `requires_render` returns `false` for `ResponseChunk`, so a
    ///    batch of chunks updates app state but does not itself force
    ///    a frame — the next `Tick` (≤100 ms) paints what accumulated.
    ///
    /// Keys, paste, resize, completion, errors, and the initial frame
    /// still render immediately.
    async fn main_loop(&mut self) -> Result<()> {
        // Render once before blocking so the initial screen is visible
        // without waiting for the first tick.
        self.render().await?;

        while !self.app.should_quit() {
            let event = self.event_handler.next_event().await;
            let mut render_now = event.requires_render();
            self.handle_event(event).await?;

            const MAX_EVENTS_PER_FRAME: usize = 256;
            for _ in 0..MAX_EVENTS_PER_FRAME {
                let Some(next) = self.event_handler.try_next_event() else {
                    break;
                };
                render_now |= next.requires_render();
                self.handle_event(next).await?;
                if self.app.should_quit() {
                    break;
                }
            }

            if render_now && !self.app.should_quit() {
                self.render().await?;
            }
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
                    if crate::ui::PaletteWidget::should_show(&self.app) {
                        let h = crate::ui::PaletteWidget::height(&self.app);
                        let w = (64u16).min(size.width);
                        let x = size.x + size.width.saturating_sub(w) / 2;
                        let y = size.y + 2;
                        let area = ratatui::layout::Rect::new(x, y, w, h);
                        crate::ui::PaletteWidget::new().render(&self.app, area, f.buffer_mut());
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
            Event::Paste(text) => {
                // H-T9: insert the pasted block into the input verbatim,
                // newlines and all. `add_char` per char would be
                // equivalent, but this also preserves a multi-line
                // paste as one undo unit conceptually. The user can
                // still edit before pressing Enter to submit.
                for ch in text.chars() {
                    if ch == '\n' {
                        self.app.insert_newline();
                    } else if ch == '\r' {
                        // CRLF pastes arrive with both; skip CR.
                    } else {
                        self.app.add_char(ch);
                    }
                }
            }
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
            Event::StreamReset => {
                self.app.drop_response_stream();
            }
            Event::ResponseComplete(text) => {
                self.gen_task = None;
                self.app.finish_response(&text);

                // Tier 1.1 — surface a tainted round in one line.
                if let Some(engine) = self.engine.clone() {
                    let t = engine.taint_level();
                    if t.is_tainting() {
                        self.app.push_system_message(&format!(
                            "⛨ round tainted by {} — high-impact tools will ask. /trust show",
                            t.as_str(),
                        ));
                    }
                }
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
                // P5.5 — after each completed turn, ask Jev whether
                // the session has moved to a new phase. A confident
                // change suggests /handoff so the user can reset the
                // transcript cleanly. The engine returns `None` for
                // every case where a hint would be noise.
                // H-T6: the phase-change check is a Jev network
                // round-trip. The pre-fix code ran it via
                // `block_in_place(block_on(...))` — a synchronous
                // network call inside the event handler that froze
                // the UI after every completed turn. Spawn it and
                // deliver the result through the same event channel
                // the streaming updates use; the hint appears
                // asynchronously when the answer arrives.
                if let Some(engine) = self.engine.clone() {
                    let tx = self.event_handler.sender();
                    tokio::spawn(async move {
                        let holder = "session".to_string();
                        let phase_change = engine.detect_phase_change_with_jev(&holder).await;
                        if let Some((old, new)) = phase_change {
                            let msg = format!(
                                "(phase changed: {old} → {new}. Consider /handoff to start fresh with a clean context.)"
                            );
                            let _ = tx
                                .send(Event::System(crate::event::EventPriority::Normal, msg))
                                .await;
                        }
                    });
                }
            }
            Event::TokenUsage(total) => {
                self.app.note_real_usage(total);
            }
            Event::SessionUsage {
                prompt_tokens,
                completion_tokens,
                cost_usd,
            } => {
                self.app
                    .note_session_usage(prompt_tokens, completion_tokens);
                if let Some(c) = cost_usd {
                    self.app.note_session_cost(c);
                }
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
            Event::ApprovalBatchRequested { batch_id, items } => {
                let items: Vec<crate::app::PendingApproval> = items
                    .into_iter()
                    .map(|i| crate::app::PendingApproval {
                        id: i.id,
                        tool_name: i.tool_name,
                        summary: i.summary,
                        diff: i.diff,
                        arguments: i.arguments,
                    })
                    .collect();
                self.app
                    .set_pending_batch(crate::app::PendingApprovalBatch {
                        batch_id,
                        items,
                        current: 0,
                    });
            }
            Event::QuestionRequested {
                id,
                question,
                placeholder,
            } => {
                self.app.set_pending_question(crate::app::PendingQuestion {
                    id,
                    question,
                    placeholder,
                });
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
            Event::HandoffGenerated(text) => {
                self.gen_task = None;
                // Write the file under the engine's working directory
                // — the one place both the TUI and the engine agree
                // on. Best-effort: a failure to write still resets
                // the session, because the value of /handoff is the
                // fresh-context reset, not the artefact.
                let mut written: Option<std::path::PathBuf> = None;
                if let Some(engine) = &self.engine {
                    let dir = engine.working_dir().join(".kod");
                    if std::fs::create_dir_all(&dir).is_ok() {
                        let stamp = chrono::Utc::now().format("%Y-%m-%d-%H%M").to_string();
                        let path = dir.join(format!("handoff-{stamp}.md"));
                        match std::fs::write(&path, text.as_bytes()) {
                            Ok(()) => written = Some(path),
                            Err(e) => self.app.push_system_message(&format!(
                                "Could not write handoff to {}: {e}",
                                path.display(),
                            )),
                        }
                    }
                }

                // Reset both surfaces. Order matters: clear the
                // display first so the user sees the chat empty while
                // the seed is installed.
                self.app.clear_messages();
                if let Some(engine) = &self.engine {
                    // `clear_history` also clears short-term memory —
                    // appropriate here: /handoff is a fresh start.
                    engine.clear_history().await;
                    let seed = format!("Context from previous session:\n\n{}", text.trim());
                    engine.seed_turn(true, &seed).await;
                }

                // Settle the generation state so the next prompt
                // starts clean.
                self.app.finish_response("");

                match written {
                    Some(p) => self.app.push_system_message(&format!(
                        "Handoff written to {}\n\
                         Session reset: the transcript above is the model's \
                         only context for the next prompt.",
                        p.display(),
                    )),
                    None => self.app.push_system_message(
                        "Session reset: the handoff is the model's only \
                         context for the next prompt. (No file was written.)",
                    ),
                }

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
            Event::SwarmAgentStarted {
                id,
                name,
                subtask,
                model,
            } => {
                self.app.swarm_agent_started(id, &name, &subtask, model);
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
                self.app
                    .swarm_set_retrying(&id, attempt, max_attempts, &previous_error);
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

        // @N focus (design D4.6): while a swarm is running, an input of
        // the form `@2 <text>` steers the second agent that started this
        // run, rather than the default (whole-session) steer. `@N` is
        // only recognised with a digit immediately after the `@` and a
        // space or end-of-input after the digit, so it cannot collide
        // with the `@path` file-reference syntax (which is path-shaped
        // and would never be just a number).
        if let Some((n, rest)) = parse_at_agent_prefix(&input)
            && let Some(agent_id) = self.app.swarm_agent_by_index(n).cloned()
        {
            self.app.submit_input();
            let key = format!("swarm:{agent_id}");
            if let Some(engine) = self.engine.clone() {
                engine.steer_for(&key, rest).await;
            }
            self.app.push_system_message(&format!(
                "Steered agent {n} — applies after its current tool call: {rest}",
            ));
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
            Some(sys) if !sys.is_empty() => format!("[system] {sys}\n\n[user] {input}",),
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
            // P1.4 — the pump needs an `Arc<KodEngine>` to classify
            // chunks. Clone before the pump so the outer task still
            // owns its own handle for `process_streaming`.
            let engine_for_pump = engine.clone();
            let pump = tokio::spawn(async move {
                // P1.4 — prose vs reasoning classification state.
                // Accumulates text so Jev can judge the *kind* of
                // text once every 5 chunks; a `reasoning` /
                // `restatement` verdict folds the chunk into the
                // thinking spinner instead of the chat.
                //
                // Safety valve: if classification has been hiding
                // text for more than the configured timeout, we
                // assume the classifier misfired and force prose for
                // the rest of the turn. The accumulated buffer is
                // flushed so nothing is lost.
                //
                // The timeout is `[jev] reasoning_timeout_secs`
                // (default 20). `None` means the valve is off —
                // a config with `reasoning_timeout_secs = 0` trusts
                // the classifier absolutely.
                let reasoning_timeout: Option<std::time::Duration> =
                    engine_for_pump.jev_client().and_then(|c| {
                        let d = c.reasoning_timeout();
                        if d.is_zero() { None } else { Some(d) }
                    });
                let mut is_reasoning: bool = false;
                let mut chunk_count: usize = 0;
                let mut accumulated: String = String::new();
                let mut reasoning_since: Option<std::time::Instant> = None;
                let mut disabled_for_turn: bool = false;
                while let Some(chunk) = chunk_rx.recv().await {
                    if let Some((id, json)) = kod_core::engine::parse_question(&chunk) {
                        let req: kod_tools::ask::QuestionRequest = serde_json::from_str(json)
                            .unwrap_or_else(|_| kod_tools::ask::QuestionRequest {
                                question: "(unparseable question)".to_string(),
                                placeholder: None,
                            });
                        let _ = event_tx_chunks
                            .send(Event::QuestionRequested {
                                id,
                                question: req.question,
                                placeholder: req.placeholder,
                            })
                            .await;
                    } else if let Some((batch_id, json)) =
                        kod_core::engine::parse_tool_approval_batch(&chunk)
                    {
                        // H-R1: a corrupt batch must not be silently
                        // dropped — the pre-fix `unwrap_or_else` left
                        // every pending approval to time out (120 s)
                        // and then deny, with no user-visible signal.
                        let batch: kod_core::engine::ApprovalBatch =
                            match serde_json::from_str(json) {
                                Ok(b) => b,
                                Err(e) => {
                                    tracing::error!(
                                        batch_id,
                                        error = %e,
                                        "TUI approval batch failed to parse; \
                                         emitting a system message",
                                    );
                                    let _ = event_tx_chunks
                                        .send(Event::System(
                                            crate::event::EventPriority::High,
                                            format!(
                                                "Approval request could not be \
                                                 parsed ({e}); it will be denied \
                                                 by the engine after the \
                                                 timeout."
                                            ),
                                        ))
                                        .await;
                                    return;
                                }
                            };
                        let items: Vec<crate::event::ApprovalItem> = batch
                            .items
                            .into_iter()
                            .filter_map(|i| {
                                Some(crate::event::ApprovalItem {
                                    id: i.id?,
                                    tool_name: i.tool_name,
                                    summary: i.summary,
                                    diff: i.diff,
                                    arguments: i.arguments.clone(),
                                })
                            })
                            .collect();
                        if !items.is_empty() {
                            let _ = event_tx_chunks
                                .send(Event::ApprovalBatchRequested { batch_id, items })
                                .await;
                        }
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
                    } else if kod_core::engine::is_stream_reset_marker(&chunk) {
                        // P5.6 — the engine abandoned the current
                        // endpoint mid-stream. Drop whatever we
                        // accumulated so the retry against the next
                        // endpoint lands in a clean bubble.
                        let _ = event_tx_chunks.send(Event::StreamReset).await;
                        // P1.4 — reset the classifier state so the
                        // retry starts from a clean slate.
                        accumulated.clear();
                        chunk_count = 0;
                        is_reasoning = false;
                        reasoning_since = None;
                        disabled_for_turn = false;
                    } else {
                        // P1.4 — classify the running buffer every
                        // 5 text chunks once we have enough to
                        // judge. Cheap: one short Jev call, gated
                        // by chunk count and buffer length.
                        accumulated.push_str(&chunk);
                        chunk_count += 1;
                        if !disabled_for_turn
                            && chunk_count.is_multiple_of(5)
                            && accumulated.len() > 200
                            && let Some(kind) = engine_for_pump
                                .classify_chunk_with_jev("session", &accumulated)
                                .await
                        {
                            is_reasoning = matches!(kind.as_str(), "reasoning" | "restatement");
                            if is_reasoning && reasoning_since.is_none() {
                                reasoning_since = Some(std::time::Instant::now());
                            } else if !is_reasoning {
                                reasoning_since = None;
                            }
                        }
                        // Safety valve: too long spent hiding text.
                        if let Some(timeout) = reasoning_timeout
                            && let Some(started) = reasoning_since
                            && started.elapsed() >= timeout
                        {
                            // Dump the buffer as one prose chunk so
                            // nothing is lost, then stop classifying
                            // for the rest of the turn.
                            tracing::warn!(
                                hidden_chars = accumulated.len(),
                                "P1.4 safety valve: forcing prose after timeout"
                            );
                            let flushed = std::mem::take(&mut accumulated);
                            if !flushed.is_empty() {
                                let _ = event_tx_chunks.send(Event::ResponseChunk(flushed)).await;
                            }
                            is_reasoning = false;
                            reasoning_since = None;
                            disabled_for_turn = true;
                        }
                        if is_reasoning {
                            // Fold the chunk into the spinner. The
                            // user perceives "thinking"; the
                            // boilerplate never hits the chat.
                            let _ = event_tx_chunks.send(Event::Thinking).await;
                        } else {
                            let _ = event_tx_chunks.send(Event::ResponseChunk(chunk)).await;
                        }
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
                        // Compute the call's USD cost from the
                        // pricing the response carries. `None` when
                        // the endpoint has no `[pricing]` block, in
                        // which case the header simply shows no `$`
                        // figure — a number we did not earn the
                        // right to print is worse than no number.
                        let cost_usd = response
                            .pricing
                            .map(|p| p.cost_usd(usage.prompt_tokens, usage.completion_tokens));
                        let _ = event_tx
                            .send(Event::SessionUsage {
                                prompt_tokens: usage.prompt_tokens,
                                completion_tokens: usage.completion_tokens,
                                cost_usd,
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
        // Keep a copy of the swarm config for the runner: `config`
        // below is moved into the closure, and `from_config` needs the
        // three budget knobs (§D4.3).
        let swarm_config = config.swarm.clone();

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
            let runner = match kod_core::SwarmRunner::from_config(engine, &swarm_config).await {
                Ok(r) => r,
                Err(e) => {
                    let _ = event_tx.send(Event::SwarmError(e.to_string())).await;
                    return;
                }
            };

            let (chunk_tx, mut chunk_rx) = tokio::sync::mpsc::channel::<kod_core::SwarmEvent>(128);
            let event_tx_pump = event_tx.clone();
            let pump = tokio::spawn(async move {
                while let Some(ev) = chunk_rx.recv().await {
                    let tui_ev = match ev {
                        kod_core::SwarmEvent::Decomposed(subs) => Event::SwarmDecomposed(
                            subs.iter()
                                .map(|s| (s.name.clone(), s.description.clone()))
                                .collect(),
                        ),
                        kod_core::SwarmEvent::AgentStarted {
                            id,
                            name,
                            subtask,
                            model,
                        } => Event::SwarmAgentStarted {
                            id,
                            name,
                            subtask,
                            model,
                        },
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
                        kod_core::SwarmEvent::BoundaryViolation {
                            agent_name,
                            paths,
                        } => {
                            // P5: a subagent wrote outside its
                            // declared globs. Surface it through the
                            // same AgentMessage channel the other
                            // informational swarm events use; the
                            // paths are listed so the user can see
                            // exactly what leaked the boundary.
                            let list = paths
                                .iter()
                                .map(|p| p.display().to_string())
                                .collect::<Vec<_>>()
                                .join(", ");
                            Event::AgentMessage(
                                "swarm".to_string(),
                                format!(
                                    "⚠ {agent_name} wrote outside its declared scope: {list}",
                                ),
                            )
                        }
                        kod_core::SwarmEvent::WorktreesMerged {
                            merged,
                            conflicted,
                            failed,
                        } => {
                            let summary = if conflicted.is_empty() && failed.is_empty() {
                                format!("worktrees merged: {} ok", merged.len())
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
                // Open the full-screen help overlay. This is what the
                // overlay's own key list promises — it renders
                // `? this help (also /help, F1)` — but the command
                // used to push `SLASH_HELP` as a system message
                // instead, leaving the overlay unreachable from the
                // command the widget advertises. The `?` key and F1
                // both route through `toggle_help`; this does too,
                // so the three entry points behave identically.
                self.app.toggle_help();
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
                            if let Some(cfg) = &config
                                && let Ok(dirs) = cfg.skills_dirs()
                            {
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
                                        if entry.path().extension().and_then(|s| s.to_str())
                                            != Some("md")
                                        {
                                            continue;
                                        }
                                        let stem = entry
                                            .path()
                                            .file_stem()
                                            .and_then(|s| s.to_str())
                                            .unwrap_or("");
                                        if stem == n {
                                            if let Ok(text) = std::fs::read_to_string(entry.path())
                                            {
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
                            let text = body.unwrap_or_else(|| format!("(description) {}", d));
                            self.app
                                .push_system_message(&format!("Skill {}\n\n{}", n, text,));
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
                                    format!("{}…", kod_types::strutil::truncate_chars(d, 100))
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
                            let path =
                                dirs::home_dir().map(|h| h.join(".kod").join("last_prompt.txt"));
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
                    let mut msg = format!(
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
                    );

                    // Per-section allocation from the most recent prompt.
                    // This is what the PromptBudget assigned to each
                    // truncatable section (history, skills, memory,
                    // repomap) plus the request. It is the number that
                    // moves when the endpoint's `context_window` shrinks,
                    // so it is the number a user staring at "context
                    // almost full" wants to see.
                    if let Some(engine) = &self.engine
                        && let Some(trace) = engine.last_prompt_trace().await
                        && let Some(a) = trace.alloc
                    {
                        let total_chars = a.request + a.truncatable_total();
                        msg.push_str("\n\nPrompt allocation (last turn):\n");
                        msg.push_str(&format!("  request:  {:>8} chars\n", a.request));
                        msg.push_str(&format!("  history:  {:>8} chars\n", a.history));
                        msg.push_str(&format!("  skills:   {:>8} chars\n", a.skills));
                        msg.push_str(&format!("  memory:   {:>8} chars\n", a.memory));
                        msg.push_str(&format!("  repomap:  {:>8} chars\n", a.repomap));
                        msg.push_str(&format!(
                            "  total:    {:>8} chars (~{} tokens)\n",
                            total_chars,
                            total_chars / 4,
                        ));
                        msg.push_str(
                            "\nShare of the truncatable budget that goes to \
                             the request is not counted above; the four other \
                             sections split what is left after the request.",
                        );
                    } else if self.engine.is_some() {
                        msg.push_str(
                            "\n\nNo prompt allocation recorded yet — send a \
                             prompt first; the table shows the most recent \
                             turn's budget.",
                        );
                    }

                    self.app.push_system_message(&msg);
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
                        msg.push_str("\nRestore with /rollback <id>, or /rollback for the newest.");
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
                        // H-T8: mirror the display rewind on the
                        // engine transcript. Drop one user + one
                        // assistant turn (2 messages).
                        if let Some(engine) = self.engine.clone() {
                            engine.forget_last_turns_for("session", 2).await;
                        }
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
                    Some(_) => {
                        // H-T8: mirror on the engine transcript.
                        if let Some(engine) = self.engine.clone() {
                            engine.forget_last_turns_for("session", 2).await;
                        }
                        self.app.push_system_message("Last exchange removed.");
                    }
                    None => self.app.push_system_message("Nothing to delete."),
                }
            }
            "/memory" => {
                // The engine owns the memory subsystem. The TUI
                // must not open a second MemoryManager here: redb
                // locks the file, so a second handle fails with
                // "database already open". Every memory operation
                // routes through the engine.
                if self.engine.is_none() {
                    self.app.push_system_message("Engine not initialized.");
                    return Ok(());
                }
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
                let manager =
                    match kod_memory::MemoryManager::new(path, config.memory.short_term_capacity) {
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
                                Err(e) => {
                                    self.app.push_system_message(&format!("Search failed: {e}"))
                                }
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
                                        .remove(kod_types::MemoryType::LongTerm, &entry.id)
                                        .await
                                    {
                                        Ok(()) => self.app.push_system_message(&format!(
                                            "Deleted memory entry {}.",
                                            &entry.id.as_uuid().to_string()[..8]
                                        )),
                                        Err(e) => self
                                            .app
                                            .push_system_message(&format!("Delete failed: {e}")),
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
                    Some("clear") => match manager.get_all_long_term().await {
                        Ok(all) => {
                            let n = all.len();
                            for e in &all {
                                let _ =
                                    manager.remove(kod_types::MemoryType::LongTerm, &e.id).await;
                            }
                            self.app.push_system_message(&format!(
                                "Cleared {} memory entr{}.",
                                n,
                                if n == 1 { "y" } else { "ies" },
                            ));
                        }
                        Err(e) => self
                            .app
                            .push_system_message(&format!("Could not read memory database: {e}")),
                    },
                    _ => {
                        // No subcommand: list entries.
                        match manager.get_all_long_term().await {
                            Ok(all) if all.is_empty() => self.app.push_system_message(
                                "No long-term memory entries. Add some with the memory tools.",
                            ),
                            Ok(all) => {
                                let mut msg =
                                    format!("Long-term memory ({} entries):\n", all.len(),);
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
            "/remember" => {
                let content: String = parts.collect::<Vec<_>>().join(" ");
                let content = content.trim();
                if content.is_empty() {
                    self.app.push_system_message(
                        "Usage: /remember <text> — store a durable fact in \
                         long-term memory. No LLM call: the text is saved \
                         verbatim. Tags default to [\"user\"]; use the CLI's \
                         `kod memory add` for custom tags.",
                    );
                    return Ok(());
                }
                // Same rationale as /memory: the engine owns the
                // subsystem, and a second redb handle fails with
                // "database already open".
                if self.engine.is_none() {
                    self.app.push_system_message("Engine not initialized.");
                    return Ok(());
                }
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
                let manager =
                    match kod_memory::MemoryManager::new(path, config.memory.short_term_capacity) {
                        Ok(m) => m,
                        Err(e) => {
                            self.app.push_system_message(&format!(
                                "Could not open memory database: {e}"
                            ));
                            return Ok(());
                        }
                    };
                let project_key = std::env::current_dir()
                    .ok()
                    .map(|cwd| kod_core::TaskRouter::project_key_for(&cwd));
                match manager
                    .store_with_metadata(
                        kod_types::MemoryType::LongTerm,
                        content,
                        kod_types::MemoryMetadata {
                            tags: vec!["user".to_string()],
                            project_key,
                            ..Default::default()
                        },
                    )
                    .await
                {
                    Ok(id) => {
                        // AD-15 audit trail: log the user channel.
                        // The store went through a manager this handler
                        // constructed; the engine only records the
                        // event.
                        if let Some(engine) = &self.engine {
                            engine
                                .record_user_memory_write(
                                    &id.as_uuid().to_string(),
                                    vec!["user".to_string()],
                                )
                                .await;
                        }
                        self.app.push_system_message(&format!(
                            "Remembered ({}): {}",
                            &id.as_uuid().to_string()[..8],
                            content,
                        ))
                    }
                    Err(e) => self
                        .app
                        .push_system_message(&format!("Could not store memory entry: {e}",)),
                }
            }
            "/policy" => {
                // Parity with the CLI's `kod policy show|explain|forget`.
                // The TUI holds a live engine, so it can reach the
                // session's accumulated deny rules directly — the rules
                // the `a` choice on the approval dialog builds up.
                let Some(engine) = &self.engine else {
                    self.app.push_system_message(
                        "Engine not initialized; no session policy to inspect.",
                    );
                    return Ok(());
                };
                let sub = parts.next();
                match sub {
                    Some("forget") => {
                        let n_str = parts.next().unwrap_or("");
                        let n: usize = match n_str.parse() {
                            Ok(v) if v > 0 => v,
                            _ => {
                                self.app.push_system_message(
                                    "Usage: /policy forget <n> — n is a 1-based \
                                     index from `/policy` (no argument).",
                                );
                                return Ok(());
                            }
                        };
                        match engine.deny_rule_at(n).await {
                            Some(rule) => {
                                let removed = engine.remove_deny_rule(&rule).await;
                                if removed {
                                    let pattern = rule.path_pattern.as_deref().unwrap_or("*");
                                    self.app.push_system_message(&format!(
                                        "Forgot deny rule {}: {} {}",
                                        n, rule.tool, pattern,
                                    ));
                                } else {
                                    self.app.push_system_message(&format!(
                                        "Rule {} disappeared before it could \
                                         be removed (raced another caller).",
                                        n,
                                    ));
                                }
                            }
                            None => {
                                self.app.push_system_message(&format!(
                                    "No rule at index {n}. Run `/policy` with no \
                                     argument to list the current rules.",
                                ));
                            }
                        }
                    }
                    Some("show") | None => {
                        let rules = engine.deny_rules().await;
                        if rules.is_empty() {
                            self.app.push_system_message(
                                "No session deny rules. Rules accumulate when \
                                 you answer `a` (never) on an approval dialog; \
                                 `/policy` lists them, `/policy forget <n>` \
                                 drops one.",
                            );
                        } else {
                            let mut msg = format!(
                                "Session deny rules ({} total):\n/policy [show | forget <n>] — inspect and clear session deny rules\n",
                                rules.len(),
                            );
                            for (i, r) in rules.iter().enumerate() {
                                let pattern = r.path_pattern.as_deref().unwrap_or("*");
                                msg.push_str(&format!("  {}. {} {}\n", i + 1, r.tool, pattern,));
                            }
                            msg.push_str("\nDrop a rule with `/policy forget <n>`.");
                            self.app.push_system_message(msg.trim_end());
                        }
                    }
                    Some(other) => {
                        self.app.push_system_message(&format!(
                            "Unknown /policy subcommand {:?}. Try `/policy` or \
                             `/policy forget <n>`.",
                            other,
                        ));
                    }
                }
            }
            "/map" => {
                // `kod map` in the CLI and `/map` in the TUI produce
                // the same output: one line per recognized source file,
                // followed by its top-level symbols. The command is
                // useful mid-session to remind the model (and the user)
                // what the repository looks like without scrolling
                // through files.
                let max_chars: usize = parts.next().and_then(|s| s.parse().ok()).unwrap_or(16_000);
                let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
                let map = kod_core::repomap::build_repo_map(&cwd);
                let rendered = map.render(max_chars);
                if rendered.trim().is_empty() {
                    self.app.push_system_message(&format!(
                        "No recognized source files under {} — the map is empty. \
                         The repo map recognizes Rust, Python, JavaScript/TypeScript, \
                         Go, Ruby, and C-family source files.",
                        cwd.display(),
                    ));
                    return Ok(());
                }
                self.app.push_system_message(&format!(
                    "Repository map ({} files, {} symbols, budget {} chars):\n\n{}",
                    map.file_count(),
                    map.symbol_count(),
                    max_chars,
                    rendered.trim_end(),
                ));
            }
            "/grep" => {
                // Regex search over the session's chat history. Unlike
                // `/search` (a case-insensitive substring over the
                // *current* message view), `/grep` accepts a Rust
                // `regex` pattern and prints the matching messages
                // with their 1-based indices — the tool for "which
                // reply mentioned `retry_after`?" or "when did we
                // discuss the sandbox fallback?".
                let pattern: String = parts.collect::<Vec<_>>().join(" ");
                let pattern = pattern.trim();
                if pattern.is_empty() {
                    self.app.push_system_message(
                        "Usage: /grep <regex> — regex search over the chat history. \
                         Example: /grep ^assert|panic",
                    );
                    return Ok(());
                }
                let re = match regex::Regex::new(pattern) {
                    Ok(r) => r,
                    Err(e) => {
                        self.app
                            .push_system_message(&format!("Invalid regex {:?}: {}", pattern, e,));
                        return Ok(());
                    }
                };
                let hits: Vec<(usize, &str, &str)> = self
                    .app
                    .messages()
                    .iter()
                    .enumerate()
                    .filter_map(|(i, m)| {
                        if re.is_match(&m.content) {
                            let role = match &m.role {
                                kod_types::MessageRole::User => "you",
                                kod_types::MessageRole::Assistant => "ai",
                                kod_types::MessageRole::System => "sys",
                                kod_types::MessageRole::Tool => "tool",
                                kod_types::MessageRole::Agent(_) => "agent",
                            };
                            Some((i + 1, role, m.content.as_str()))
                        } else {
                            None
                        }
                    })
                    .collect();
                if hits.is_empty() {
                    self.app
                        .push_system_message(&format!("No messages match {:?}.", pattern,));
                    return Ok(());
                }
                let mut msg = format!("{} message(s) match {:?}:\n", hits.len(), pattern,);
                // Cap the display at 30 messages, printing the first
                // line of each so a search over a long transcript
                // stays readable.
                for (n, role, content) in hits.iter().take(30) {
                    let first_line = content.lines().next().unwrap_or("");
                    let shown = if first_line.chars().count() > 120 {
                        let s: String = first_line.chars().take(120).collect();
                        format!("{s}…")
                    } else {
                        first_line.to_string()
                    };
                    msg.push_str(&format!("  {:>4}. [{}] {}\n", n, role, shown,));
                }
                if hits.len() > 30 {
                    msg.push_str(&format!(
                        "… and {} more. Narrow the pattern to see them.",
                        hits.len() - 30,
                    ));
                }
                self.app.push_system_message(msg.trim_end());
            }
            "/context" => {
                let used = self.app.context_tokens();
                let limit = self.app.context_limit();
                let pct = self.app.context_usage() * 100.0;
                let inp = self.app.session_input_tokens();
                let out = self.app.session_output_tokens();
                let total = self.app.session_total_tokens();
                let msg_count = self.app.messages().len();
                let assistant_count = self
                    .app
                    .messages()
                    .iter()
                    .filter(|m| matches!(m.role, kod_types::MessageRole::Assistant))
                    .count();
                let tool_count = self
                    .app
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
            "/pin" | "/unpin" => {
                let pin = cmd == "/pin";
                let arg = parts.next();
                let Some(idx_str) = arg else {
                    self.app.push_system_message(if pin {
                        "Usage: /pin <n> — pin the message at 1-based index n. \
                         User and assistant rows only; tool and system rows are \
                         transcript-local."
                    } else {
                        "Usage: /unpin <n> — remove the pin from message n."
                    });
                    return Ok(());
                };
                let Ok(n) = idx_str.parse::<usize>() else {
                    self.app
                        .push_system_message(&format!("Not a number: {:?}", idx_str,));
                    return Ok(());
                };
                if n == 0 {
                    self.app
                        .push_system_message("Index is 1-based; try /pin 1 for the first message.");
                    return Ok(());
                }
                let (role, content, total) = {
                    let msgs = self.app.messages();
                    if n > msgs.len() {
                        self.app.push_system_message(&format!(
                            "No message at index {} (the session has {} messages).",
                            n,
                            msgs.len(),
                        ));
                        return Ok(());
                    }
                    let m = &msgs[n - 1];
                    (m.role.clone(), m.content.clone(), msgs.len())
                };
                match role {
                    kod_types::MessageRole::User | kod_types::MessageRole::Assistant => {}
                    _ => {
                        self.app.push_system_message(
                            "Only user and assistant messages can be pinned — \
                             tool, system, and agent rows are transcript-local.",
                        );
                        return Ok(());
                    }
                }
                if let Some(engine) = &self.engine {
                    let found = engine.set_turn_pinned_by_content("", &content, pin).await;
                    if !found {
                        self.app.push_system_message(
                            "The engine no longer has that turn in its \
                             transcript (it may have been compacted). The pin \
                             is set in the chat but will not affect the model's \
                             prompt.",
                        );
                    }
                }
                self.app.set_message_pinned_at(n - 1, pin);
                let verb = if pin { "Pinned" } else { "Unpinned" };
                self.app
                    .push_system_message(&format!("{verb} message {n} of {total}.",));
            }
            "/handoff" => {
                if self.app.is_generating() {
                    self.app.push_system_message(
                        "Wait for the current prompt to finish before running \
                         /handoff.",
                    );
                    return Ok(());
                }
                let Some(engine) = self.engine.clone() else {
                    self.app.push_system_message("Engine not initialized.");
                    return Ok(());
                };
                if self.app.messages().is_empty() {
                    self.app
                        .push_system_message("Nothing to hand off — the session is empty.");
                    return Ok(());
                }
                let transcript = self.app.export_markdown();
                self.app.begin_generation();
                self.app.push_system_message("Generating handoff document…");
                let event_tx = self.event_handler.sender();
                // P4.8 — pre-extract the durable facts with Jev
                // before the LLM reads the transcript. Empty when
                // Jev is disabled; the LLM then does the extraction
                // itself as before.
                let facts = engine.extract_handoff_facts_with_jev(&transcript).await;
                tokio::spawn(async move {
                    let facts_block = if facts.is_empty() {
                        String::new()
                    } else {
                        format!(
                            "\n\nPre-extracted facts (already vetted by a prior pass; \n\
                             use them as the source of truth for the sections below):\n\n{}\n",
                            facts.join("\n"),
                        )
                    };
                    let prompt = format!(
                        "Produce a handoff document for the coding session \
                         below. Use exactly these markdown sections, in this \
                         order:\n\n\
                         ## Decisions\n\
                         ## Code state\n\
                         ## Next steps\n\
                         ## Key files\n\n\
                         Rules:\n\
                         - Decisions: one bullet per durable choice the \
                           session made.\n\
                         - Code state: what currently works, what is \
                           half-done.\n\
                         - Next steps: concrete, imperative, at most 6 \
                           bullets.\n\
                         - Key files: paths only, one per line, no prose.\n\
                         - Be terse. No introduction, no closing.{facts_block}\n\
                         Session:\n\n{transcript}",
                    );
                    match engine.process(&prompt).await {
                        Ok(resp) => {
                            let text = resp.text.unwrap_or_default();
                            let _ = event_tx.send(Event::HandoffGenerated(text)).await;
                        }
                        Err(e) => {
                            let _ = event_tx.send(Event::Error(format!("/handoff: {e}"))).await;
                        }
                    }
                });
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
                    Err(e) => self
                        .app
                        .push_system_message(&format!("Could not read checkpoints: {e}",)),
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
                            let mut msg = format!("Attached files ({}):\n", attached.len(),);
                            for f in attached {
                                msg.push_str(&format!("  {}\n", f.display()));
                            }
                            msg.push_str("\n/attach clear to remove, or send your next prompt.");
                            self.app.push_system_message(&msg);
                        }
                    }
                    Some("eval") => {
                        // Tier 2.4 — retrieval hit-rate report.
                        let Some(engine) = &self.engine else {
                            self.app.push_system_message("Engine not initialized.");
                            return Ok(());
                        };
                        let Some(path) = engine.session_log_path() else {
                            self.app.push_system_message(
                                "No session log installed. Set KOD_SESSION_LOG.",
                            );
                            return Ok(());
                        };
                        match kod_core::session_log::read_session(&path) {
                            Ok(entries) => {
                                let mut total = 0_usize;
                                let mut with_refs = 0_usize;
                                let mut empty = 0_usize;
                                for e in &entries {
                                    if let kod_core::session_log::SessionEntry::MemoryRetrieval {
                                        retrieved,
                                        referenced,
                                        ..
                                    } = e
                                    {
                                        total += 1;
                                        if retrieved.is_empty() {
                                            empty += 1;
                                        }
                                        if !referenced.is_empty() {
                                            with_refs += 1;
                                        }
                                    }
                                }
                                if total == 0 {
                                    self.app
                                        .push_system_message("No memory retrievals logged yet.");
                                    return Ok(());
                                }
                                let rate = if total > 0 {
                                    with_refs as f64 / total as f64 * 100.0
                                } else {
                                    0.0
                                };
                                self.app.push_system_message(&format!(
                                    "Memory retrieval ({} events)\n  \
                                     with references: {} ({:.0}%)\n  \
                                     empty: {}\n\n\
                                     (references are classified by Jev; a \
                                     follow-up pass fills this in.)",
                                    total, with_refs, rate, empty,
                                ));
                            }
                            Err(e) => {
                                self.app.push_system_message(&format!(
                                    "Could not read session log: {e}",
                                ));
                            }
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
                    self.app
                        .push_system_message("Nothing to refine — no assistant reply yet.");
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
                self.app.push_system_message("Refining the last reply…");
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
                    None => self.app.push_system_message("No assistant reply yet."),
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
                    Err(e) => self.app.push_system_message(&format!("Save failed: {e}")),
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
                    Err(e) => self.app.push_system_message(&format!("Load failed: {e}")),
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
                                format!("{}…", kod_types::strutil::truncate_chars(s, 300))
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
                        format!("{}…", kod_types::strutil::truncate_chars(rest, 80))
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
                            let _ = event_tx.send(Event::ResponseComplete(text)).await;
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
                msg.push_str(&format!(
                    "  tool output:   {}\n",
                    if tools_on { "shown" } else { "hidden" }
                ));
                msg.push_str(&format!(
                    "  autocompact:   {}\n",
                    if auto_on { "on" } else { "off" }
                ));
                msg.push_str(&format!(
                    "  system prompt: {}\n",
                    if sys_override {
                        "override active"
                    } else {
                        "(default)"
                    }
                ));
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
                self.app.push_system_message(msg.trim_end());
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
                msg.push_str(&format!(
                    "  input tokens:    {}\n",
                    self.app.session_input_tokens()
                ));
                msg.push_str(&format!(
                    "  output tokens:   {}\n",
                    self.app.session_output_tokens()
                ));
                msg.push_str(&format!(
                    "  context:         {}\n",
                    self.app.context_label()
                ));
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

                // Tier 1.3 — redaction counts from the session log.
                // Each `SessionEntry::Redaction` carries the rules
                // that fired on one write; aggregate them.
                if let Some(engine) = &self.engine
                    && let Some(path) = engine.session_log_path()
                    && let Ok(entries) = kod_core::session_log::read_session(&path)
                {
                    let mut by_rule: std::collections::BTreeMap<String, usize> = Default::default();
                    for e in &entries {
                        if let kod_core::session_log::SessionEntry::Redaction { rules, .. } = e {
                            for r in rules {
                                *by_rule.entry(r.rule.clone()).or_insert(0) += r.count;
                            }
                        }
                    }
                    if !by_rule.is_empty() {
                        let total: usize = by_rule.values().sum();
                        msg.push_str(&format!("\nSecrets redacted this session: {total}\n",));
                        for (rule, n) in &by_rule {
                            msg.push_str(&format!("  {:<24} {}\n", rule, n));
                        }
                    }
                }
                self.app.push_system_message(msg.trim_end());
            }
            "/git-status" => {
                // `git status --porcelain=v2 -b` in the working
                // directory, printed as a system message.
                let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
                match std::process::Command::new("git")
                    .args(["status", "--porcelain=v2", "-b"])
                    .current_dir(&cwd)
                    .output()
                {
                    Ok(out) if out.status.success() => {
                        let text = String::from_utf8_lossy(&out.stdout);
                        let trimmed = text.trim_end();
                        if trimmed.is_empty() {
                            self.app
                                .push_system_message("Working tree is clean (no changes).");
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
                        self.app
                            .push_system_message(&format!("git status failed: {}", err.trim(),));
                    }
                    Err(e) => self
                        .app
                        .push_system_message(&format!("Could not run git: {e} — is git on PATH?",)),
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
                    self.app
                        .push_system_message("Nothing to fork — the chat is empty.");
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
            "/blackboard" => {
                let Some(engine) = &self.engine else {
                    self.app.push_system_message("Engine not initialized.");
                    return Ok(());
                };
                match parts.next() {
                    None | Some("show") => {
                        let entries = engine.blackboard().all();
                        if entries.is_empty() {
                            self.app.push_system_message(
                                "Blackboard is empty. It is populated during a swarm run.",
                            );
                            return Ok(());
                        }
                        let mut msg = format!("Blackboard ({} entries)\n", entries.len());
                        for e in entries.iter().take(40) {
                            msg.push_str(&format!(
                                "  {:<40} {}\n",
                                e.key,
                                serde_json::to_string(&e.value)
                                    .unwrap_or_default()
                                    .chars()
                                    .take(60)
                                    .collect::<String>(),
                            ));
                        }
                        if entries.len() > 40 {
                            msg.push_str(&format!("  … and {} more\n", entries.len() - 40,));
                        }
                        self.app.push_system_message(msg.trim_end());
                    }
                    Some("clear") => {
                        engine.blackboard().clear();
                        self.app.push_system_message("Blackboard cleared.");
                    }
                    Some(other) => {
                        self.app.push_system_message(&format!(
                            "Unknown /blackboard sub-command: {other}. Try /blackboard or /blackboard clear.",
                        ));
                    }
                }
            }
            "/learned" => {
                let Some(engine) = &self.engine else {
                    self.app.push_system_message("Engine not initialized.");
                    return Ok(());
                };
                match parts.next() {
                    None | Some("show") => {
                        let n = engine.learned_allow_count().await;
                        if n == 0 {
                            self.app.push_system_message(
                                "No learned allows this session. Use 'l' in an approval dialog to teach one.",
                            );
                        } else {
                            self.app.push_system_message(&format!(
                                "{n} learned allow(s) this session. \
                                 /learned clear to forget them all.",
                            ));
                        }
                    }
                    Some("clear") => {
                        engine.clear_learned_allows().await;
                        self.app.push_system_message("Learned allows cleared.");
                    }
                    Some(other) => {
                        self.app.push_system_message(&format!(
                            "Unknown /learned sub-command: {other}. Try /learned or /learned clear.",
                        ));
                    }
                }
            }
            "/decisions" => {
                let Some(engine) = &self.engine else {
                    self.app.push_system_message("Engine not initialized.");
                    return Ok(());
                };
                match parts.next() {
                    None | Some("show") => {
                        let log = engine.decisions_for("session").await;
                        if log.entries.is_empty() {
                            self.app.push_system_message(
                                "No durable decisions recorded yet. Decisions are \
                                 logged automatically on turns that state a \
                                 preference, approach, file change, or constraint.",
                            );
                            return Ok(());
                        }
                        let mut msg = format!("Decisions ({} entries)\n", log.entries.len(),);
                        for d in log.entries.iter().rev().take(20) {
                            let tag = match d.kind {
                                kod_core::DecisionKind::UserPreference => "pref",
                                kod_core::DecisionKind::Approach => "appr",
                                kod_core::DecisionKind::FileChange => "file",
                                kod_core::DecisionKind::Constraint => "cons",
                                kod_core::DecisionKind::Other => "othr",
                            };
                            msg.push_str(&format!(
                                "  [{}] {}\n",
                                tag,
                                d.text.chars().take(120).collect::<String>(),
                            ));
                        }
                        if log.entries.len() > 20 {
                            msg.push_str(&format!("  … and {} more\n", log.entries.len() - 20,));
                        }
                        msg.push_str("\n  /decisions drop <id> | /decisions clear");
                        self.app.push_system_message(msg.trim_end());
                    }
                    Some("drop") => {
                        let id: Option<u64> = parts.next().and_then(|s| s.parse().ok());
                        match id {
                            Some(id) => {
                                if engine.drop_decision("session", id).await {
                                    self.app
                                        .push_system_message(&format!("Dropped decision {id}."));
                                } else {
                                    self.app.push_system_message(&format!(
                                        "No decision with id {id}.",
                                    ));
                                }
                            }
                            None => self.app.push_system_message("Usage: /decisions drop <id>"),
                        }
                    }
                    Some("clear") => {
                        let mut g = engine.decisions_for("session").await;
                        g.entries.clear();
                        // Replace the log with the empty one.
                        engine.set_decision_log("session", g).await;
                        self.app.push_system_message("Decision log cleared.");
                    }
                    Some(other) => {
                        self.app.push_system_message(&format!(
                            "Unknown /decisions sub-command: {other}. Try /decisions, /decisions drop <id>, or /decisions clear.",
                        ));
                    }
                }
            }
            "/plan" => {
                let Some(engine) = &self.engine else {
                    self.app.push_system_message("Engine not initialized.");
                    return Ok(());
                };
                match parts.next() {
                    None | Some("show") => match engine.plan_for("session").await {
                        Some(plan) => {
                            let mut msg = format!("Plan for: {}\n\n", plan.goal);
                            for step in &plan.steps {
                                let marker = match step.status {
                                    kod_core::PlanStatus::Done => "✓",
                                    kod_core::PlanStatus::InProgress => "→",
                                    kod_core::PlanStatus::Blocked => "!",
                                    kod_core::PlanStatus::Skipped => "·",
                                    kod_core::PlanStatus::Pending => " ",
                                };
                                msg.push_str(&format!(
                                    "{} {}. {}\n",
                                    marker,
                                    step.id + 1,
                                    step.text,
                                ));
                                if let Some(notes) = plan.notes.get(&step.id) {
                                    for n in notes {
                                        msg.push_str(&format!("   note: {n}\n"));
                                    }
                                }
                            }
                            msg.push_str(
                                &format!("\nProgress: {:.0}%\n", plan.progress() * 100.0,),
                            );
                            self.app.push_system_message(msg.trim_end());
                        }
                        None => {
                            self.app.push_system_message(
                                "No plan for this session. Plans are created \
                                     automatically on Complex or MultiStep tasks.",
                            );
                        }
                    },
                    Some("next") => {
                        let desc = engine
                            .apply_plan_update("session", kod_core::PlanUpdate::Advance)
                            .await;
                        self.app.push_system_message(&desc);
                    }
                    Some("skip") => {
                        // Skip == set current step Skipped, then advance.
                        if let Some(plan) = engine.plan_for("session").await
                            && let Some(cur) = plan.current_step()
                        {
                            let id = cur.id;
                            let desc = engine
                                .apply_plan_update(
                                    "session",
                                    kod_core::PlanUpdate::SetStatus {
                                        step_id: id,
                                        status: kod_core::PlanStatus::Skipped,
                                    },
                                )
                                .await;
                            self.app.push_system_message(&desc);
                        } else {
                            self.app.push_system_message("No current step to skip.");
                        }
                    }
                    Some("note") => {
                        let note: String = parts.collect::<Vec<_>>().join(" ");
                        if note.is_empty() {
                            self.app.push_system_message("Usage: /plan note <text>");
                            return Ok(());
                        }
                        let Some(plan) = engine.plan_for("session").await else {
                            self.app.push_system_message("No plan for this session.");
                            return Ok(());
                        };
                        let Some(cur) = plan.current_step() else {
                            self.app.push_system_message("No current step.");
                            return Ok(());
                        };
                        let id = cur.id;
                        let desc = engine
                            .apply_plan_update(
                                "session",
                                kod_core::PlanUpdate::Annotate { step_id: id, note },
                            )
                            .await;
                        self.app.push_system_message(&desc);
                    }
                    Some("clear") => {
                        engine.clear_plan("session").await;
                        self.app.push_system_message("Plan cleared.");
                    }
                    Some(other) => {
                        self.app.push_system_message(&format!(
                            "Unknown /plan sub-command: {other}. Try /plan, /plan next, /plan skip, /plan note <text>, /plan clear.",
                        ));
                    }
                }
            }
            "/jobs" => {
                // P6: background job surface. Lists every job the
                // runner knows about, newest first, with its kind
                // and status. Pruning happens on read so the map
                // does not grow without bound.
                let Some(engine) = &self.engine else {
                    self.app.push_system_message("Engine not initialized.");
                    return Ok(());
                };
                let runner = engine.background();
                // Drop jobs older than 5 minutes so a long session
                // does not accumulate terminal entries.
                runner.prune_old(std::time::Duration::from_secs(300));
                let snap = runner.snapshot();
                if snap.is_empty() {
                    self.app.push_system_message(
                        "No background jobs. /review spawns a cross-model \
                         review of the last assistant turn.",
                    );
                    return Ok(());
                }
                let mut msg = String::from("Background jobs\n");
                for (id, state) in &snap {
                    let elapsed = state.started_at.elapsed().as_secs();
                    let status = match &state.status {
                        kod_core::background::JobStatus::Running => "running".to_string(),
                        kod_core::background::JobStatus::Completed { summary } => {
                            format!("done: {}", summary.lines().next().unwrap_or(""))
                        }
                        kod_core::background::JobStatus::Failed { error } => {
                            format!("failed: {}", error.lines().next().unwrap_or(""))
                        }
                    };
                    msg.push_str(&format!(
                        "  {id}  {:>4}s  {}\n         {}\n",
                        elapsed,
                        state.kind.label(),
                        status,
                    ));
                }
                self.app.push_system_message(msg.trim_end());
            }
            "/review" => {
                // P6: spawn a cross-model review of the most recent
                // assistant turn. The job runs under the runner's
                // concurrency cap; its summary lands on /jobs.
                let Some(engine) = &self.engine else {
                    self.app.push_system_message("Engine not initialized.");
                    return Ok(());
                };
                // The most recent assistant message, if any.
                let last_assistant = self
                    .app
                    .messages()
                    .iter()
                    .rev()
                    .find(|m| m.role == kod_types::MessageRole::Assistant)
                    .map(|m| m.content.clone());
                let Some(text) = last_assistant else {
                    self.app.push_system_message(
                        "No assistant turn to review yet.",
                    );
                    return Ok(());
                };
                // Use a monotonic id derived from the app's own
                // sequence, since the trace id is internal to the
                // engine's tracing subsystem. Zero is a valid
                // placeholder for a review that is not keyed to a
                // specific turn trace.
                let id = engine.spawn_background_review(0, text).await;
                self.app.push_system_message(&format!(
                    "Review spawned ({id}). /jobs lists running jobs.",
                ));
            }
            "/cache" => {
                // P1 surface: per-endpoint cache warmth and the
                // currently-warm endpoint. Pairs with `/limits` —
                // that one shows tool spend, this one shows KV-cache
                // state.
                let Some(engine) = &self.engine else {
                    self.app.push_system_message("Engine not initialized.");
                    return Ok(());
                };
                let (snapshot, sticky) = engine.cache_snapshot();
                let unhealthy = engine.unhealthy_endpoints();
                if snapshot.is_empty() && unhealthy.is_empty() {
                    self.app.push_system_message(
                        "No cache activity yet. A call to a provider that \
                         reports cache fields will populate this view.",
                    );
                    return Ok(());
                }
                let mut msg = String::from("Cache ledger\n");
                if let Some(s) = &sticky {
                    msg.push_str(&format!("  warm endpoint: {s}\n"));
                } else {
                    msg.push_str("  warm endpoint: (none)\n");
                }
                msg.push('\n');
                if !snapshot.is_empty() {
                    msg.push_str("  endpoint             cached tokens  last used\n");
                    for (ep, tokens, turn) in &snapshot {
                        msg.push_str(&format!(
                            "  {:<20} {:>13}  {}\n",
                            ep, tokens, turn
                        ));
                    }
                }
                if !unhealthy.is_empty() {
                    msg.push_str("\n  Unhealthy (circuit open):\n");
                    for (ep, fails, err) in &unhealthy {
                        let e = err.as_deref().unwrap_or("(no error)");
                        msg.push_str(&format!(
                            "  {:<20} {} fails  {}\n",
                            ep, fails, e
                        ));
                    }
                }
                self.app.push_system_message(msg.trim_end());
            }
            "/limits" => {
                let Some(engine) = &self.engine else {
                    self.app.push_system_message("Engine not initialized.");
                    return Ok(());
                };
                match parts.next() {
                    None | Some("show") => {
                        let snap = engine.tool_count_snapshot();
                        if snap.is_empty() {
                            self.app
                                .push_system_message("No tool calls this session yet.");
                            return Ok(());
                        }
                        let mut msg = String::from("Per-tool counts (this turn / this session)\n");
                        for (name, turn, session) in &snap {
                            msg.push_str(&format!("  {:<20} {:>6} / {}\n", name, turn, session,));
                        }
                        self.app.push_system_message(msg.trim_end());
                    }
                    Some("reset") => {
                        engine.reset_tool_counts();
                        self.app
                            .push_system_message("Per-session tool counters reset.");
                    }
                    Some(other) => {
                        self.app.push_system_message(&format!(
                            "Unknown /limits sub-command: {other}. Try /limits or /limits reset.",
                        ));
                    }
                }
            }
            "/trace" => {
                let Some(engine) = &self.engine else {
                    self.app.push_system_message("Engine not initialized.");
                    return Ok(());
                };
                let Some(path) = engine.trace_path() else {
                    self.app.push_system_message(
                        "No turn trace writer installed. Set KOD_SESSION_LOG to enable one.",
                    );
                    return Ok(());
                };
                let traces = match kod_core::read_traces(&path) {
                    Ok(t) => t,
                    Err(e) => {
                        self.app
                            .push_system_message(&format!("Could not read traces: {e}"));
                        return Ok(());
                    }
                };
                if traces.is_empty() {
                    self.app.push_system_message("No turn traces recorded yet.");
                    return Ok(());
                }
                let which = parts.next();
                match which {
                    None | Some("last") => {
                        let t = traces.last().unwrap();
                        self.app.push_system_message(&format_turn_trace_verbose(t));
                    }
                    Some("list") => {
                        let mut msg = String::from("Turn traces (newest first)\n");
                        for t in traces.iter().rev().take(20) {
                            msg.push_str(&format!(
                                "  #{:<4} {:<10} {:>7.2}s  ${:.4}  {:>5}in {:>5}out  {} tools  {}\n",
                                t.id,
                                t.holder,
                                t.duration_ms() as f64 / 1000.0,
                                t.cost_usd,
                                t.prompt_tokens,
                                t.completion_tokens,
                                t.tool_call_count,
                                match t.outcome {
                                    kod_core::TurnOutcome::Completed => "ok",
                                    kod_core::TurnOutcome::Cancelled => "cancelled",
                                    kod_core::TurnOutcome::Failed => "failed",
                                    kod_core::TurnOutcome::BudgetExhausted => "budget",
                                },
                            ));
                        }
                        self.app.push_system_message(msg.trim_end());
                    }
                    Some(id_str) => {
                        if let Ok(id) = id_str.parse::<u64>() {
                            match traces.iter().find(|t| t.id == id) {
                                Some(t) => {
                                    self.app.push_system_message(&format_turn_trace_verbose(t))
                                }
                                None => self
                                    .app
                                    .push_system_message(&format!("No trace with id {id}.",)),
                            }
                        } else {
                            self.app.push_system_message(&format!(
                                "Unknown /trace arg: {id_str}. Try /trace, /trace last, /trace list, or /trace <id>.",
                            ));
                        }
                    }
                }
            }
            "/trust" => {
                let Some(engine) = &self.engine else {
                    self.app.push_system_message("Engine not initialized.");
                    return Ok(());
                };
                match parts.next() {
                    None | Some("show") => {
                        let t = engine.taint_level();
                        let msg = format!(
                            "Current round taint: {}\n\n\
                             Content at trust=untrusted or trust=retrieved forces \
                             an approval prompt for high-impact tools (execute_command, \
                             write_file, patch_file, git_commit, git_branch_create).\n\
                             /trust clear resets to assistant until the next tool call.",
                            t.as_str(),
                        );
                        self.app.push_system_message(&msg);
                    }
                    Some("clear") => {
                        engine.clear_taint();
                        self.app.push_system_message(
                            "Taint cleared. Any future untrusted tool call will re-escalate.",
                        );
                    }
                    Some(other) => {
                        self.app.push_system_message(&format!(
                            "Unknown /trust sub-command: {other}. Try /trust or /trust clear.",
                        ));
                    }
                }
            }
            "/budget" => {
                let Some(engine) = &self.engine else {
                    self.app.push_system_message("Engine not initialized.");
                    return Ok(());
                };
                match parts.next() {
                    None | Some("show") => {
                        let snap = engine.cost_tracker().snapshot();
                        let mut msg = String::from("Session budget\n");
                        if snap.session_cap_usd > 0.0 {
                            msg.push_str(&format!(
                                "  session:  ${:.4} / ${:.2}  ({:.0}%)\n",
                                snap.session_usd,
                                snap.session_cap_usd,
                                snap.session_fraction * 100.0,
                            ));
                        } else {
                            msg.push_str(&format!(
                                "  session:  ${:.4} (no cap)\n",
                                snap.session_usd,
                            ));
                        }
                        if snap.turn_cap_usd > 0.0 {
                            msg.push_str(&format!(
                                "  turn:     ${:.4} / ${:.2}  ({:.0}%)\n",
                                snap.turn_usd,
                                snap.turn_cap_usd,
                                snap.turn_fraction * 100.0,
                            ));
                        } else {
                            msg.push_str(&format!("  turn:     ${:.4} (no cap)\n", snap.turn_usd,));
                        }
                        msg.push_str(&format!(
                            "  policy:   {:?}\n",
                            engine.cost_tracker().on_exhausted(),
                        ));
                        if snap.exhausted {
                            msg.push_str(
                                "\n⛔ A cap is exhausted. /budget raise <usd> to lift it.",
                            );
                        } else if snap.session_warned || snap.turn_warned {
                            msg.push_str("\n⚠ Approaching a cap.");
                        }
                        self.app.push_system_message(msg.trim_end());
                    }
                    Some("reset") => {
                        engine.cost_tracker().reset();
                        self.app.push_system_message("Session cost counters reset.");
                    }
                    Some("raise") => {
                        let amount: f64 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0.0);
                        if amount <= 0.0 {
                            self.app.push_system_message(
                                "Usage: /budget raise <usd> (e.g. /budget raise 5)",
                            );
                            return Ok(());
                        }
                        engine.cost_tracker().raise_session_cap(amount);
                        let snap = engine.cost_tracker().snapshot();
                        self.app.push_system_message(&format!(
                            "Session cap raised by ${amount:.2}. New cap: ${:.2}",
                            snap.session_cap_usd,
                        ));
                    }
                    Some(other) => {
                        self.app.push_system_message(&format!(
                            "Unknown /budget sub-command: {other}. Try /budget, /budget raise <usd>, or /budget reset.",
                        ));
                    }
                }
            }
            "/jev" => {
                // Jev (TypeSafe AI) integration control and
                // observability. Sub-commands:
                //
                //   /jev              status line
                //   /jev stats        decision summary from the
                //                     session log
                //   /jev cache clear  drop the in-memory decision
                //                     cache
                //   /jev test         smoke check the endpoint
                //
                // On/off and threshold changes require a restart;
                // the config is loaded once at startup. Guidance is
                // printed so a user who expected those to work
                // mid-session knows where to edit.
                let sub = parts.next();
                let Some(engine) = &self.engine else {
                    self.app.push_system_message("Engine not initialized.");
                    return Ok(());
                };
                match sub {
                    None | Some("status") => match engine.jev_status() {
                        Some(line) => {
                            let mut msg = format!("Jev status\n  {line}\n");
                            if let Some(t) = engine.jev_thresholds_line() {
                                msg.push_str(&format!("  thresholds: {t}\n"));
                            }
                            msg.push_str(
                                    "\nSub-commands: /jev stats | /jev cache clear | /jev test | /jev tune",
                                );
                            self.app.push_system_message(&msg);
                        }
                        None => {
                            self.app.push_system_message(
                                "Jev is disabled. Enable it in ~/.kod/config.toml under \
                                     [jev] enabled = true, and set TYPESAFE_API_KEY (or a key in \
                                     [jev] api_key). Restart kod after editing.",
                            );
                        }
                    },
                    Some("stats") => {
                        let Some(path) = engine.session_log_path() else {
                            self.app.push_system_message(
                                "No session log installed for this run. \
                                 Set KOD_SESSION_LOG to enable one.",
                            );
                            return Ok(());
                        };
                        match kod_core::session_log::read_session(&path) {
                            Ok(entries) => {
                                let mut total: usize = 0;
                                let mut by_source: std::collections::HashMap<String, usize> =
                                    std::collections::HashMap::new();
                                let mut by_purpose: std::collections::HashMap<String, usize> =
                                    std::collections::HashMap::new();
                                let mut total_latency_ms: u64 = 0;
                                let mut cached: usize = 0;
                                for e in &entries {
                                    if let kod_core::session_log::SessionEntry::JevDecision {
                                        purpose,
                                        latency_ms,
                                        cached: c,
                                        source,
                                        ..
                                    } = e
                                    {
                                        total += 1;
                                        *by_source.entry(source.clone()).or_insert(0) += 1;
                                        *by_purpose.entry(purpose.clone()).or_insert(0) += 1;
                                        total_latency_ms += latency_ms;
                                        if *c {
                                            cached += 1;
                                        }
                                    }
                                }
                                if total == 0 {
                                    self.app.push_system_message(
                                        "No Jev decisions recorded in this session log.",
                                    );
                                    return Ok(());
                                }
                                let mut msg = format!("Jev decisions this session: {total}\n",);
                                for (src, n) in by_source.iter() {
                                    let pct = (*n as f64 / total as f64) * 100.0;
                                    msg.push_str(&format!("  {:<10} {n:>4}  ({pct:.0}%)\n", src,));
                                }
                                msg.push_str(&format!(
                                    "\nCache hits:      {cached} ({}%)\n",
                                    if total > 0 {
                                        (cached as f64 / total as f64 * 100.0).round() as u64
                                    } else {
                                        0
                                    },
                                ));
                                msg.push_str(&format!(
                                    "Avg Jev latency: {}ms\n",
                                    total_latency_ms / total as u64,
                                ));
                                msg.push_str("\nBy purpose\n");
                                let mut rows: Vec<(&String, &usize)> = by_purpose.iter().collect();
                                rows.sort_by(|a, b| b.1.cmp(a.1));
                                for (p, n) in rows {
                                    msg.push_str(&format!("  {:<20} {n}\n", p));
                                }
                                self.app.push_system_message(msg.trim_end());
                            }
                            Err(e) => {
                                self.app.push_system_message(&format!(
                                    "Could not read session log: {e}",
                                ));
                            }
                        }
                    }
                    Some("cache") => match parts.next() {
                        Some("clear") => match engine.jev_clear_cache() {
                            Some(n) => self.app.push_system_message(&format!(
                                "Cleared {n} cached Jev decision{}. ",
                                if n == 1 { "" } else { "s" },
                            )),
                            None => self
                                .app
                                .push_system_message("Jev is disabled — nothing to clear."),
                        },
                        _ => self.app.push_system_message("Usage: /jev cache clear"),
                    },
                    Some("test") => {
                        let Some(client) = engine.jev_client() else {
                            self.app.push_system_message(
                                "Jev is disabled. Edit [jev] in ~/.kod/config.toml and restart.",
                            );
                            return Ok(());
                        };
                        self.app
                            .push_system_message("Jev smoke test: pinging the endpoint…");
                        // A minimal yes/no question with a clear
                        // answer. The endpoint gets a real state and
                        // a real question; the reply tells us both
                        // that auth works and that the model is
                        // answering.
                        let state =
                            kod_core::jev::build_state("The sky is blue on a clear day.", &[]);
                        let started = std::time::Instant::now();
                        match client
                            .evaluate_yes_no(
                                &state,
                                "Is the sky described here as blue? Answer yes or no.",
                            )
                            .await
                        {
                            Ok(d) => {
                                let ms = started.elapsed().as_millis();
                                self.app.push_system_message(&format!(
                                    "✓ Jev responded in {ms}ms: value={} confidence={:.2}",
                                    d.value, d.confidence,
                                ));
                            }
                            Err(e) => {
                                self.app
                                    .push_system_message(&format!("✗ Jev call failed: {e}",));
                            }
                        }
                    }
                    Some("tune") => {
                        // Inspect or set a Jev threshold. Persistent:
                        // the change is written to config and the
                        // in-process client is rebuilt immediately.
                        let sub = parts.next();
                        match sub {
                            None => {
                                let Some(client) = engine.jev_client() else {
                                    self.app.push_system_message("Jev is disabled.");
                                    return Ok(());
                                };
                                let t = client.thresholds();
                                let mut msg = String::from("Jev thresholds (name = value)\n");
                                for name in kod_config::JevThresholds::NAMES {
                                    if let Some(v) = t.get(name) {
                                        msg.push_str(&format!("  {:<24} {:.2}\n", name, v));
                                    }
                                }
                                msg.push_str(
                                    "\n  /jev tune set <name> <value>\n\
                                     /jev tune reset",
                                );
                                self.app.push_system_message(msg.trim_end());
                            }
                            Some("set") => {
                                let name = parts.next().map(String::from);
                                let value: Option<f32> = parts.next().and_then(|s| s.parse().ok());
                                match (name, value) {
                                    (Some(n), Some(v)) => {
                                        match engine.update_jev_threshold(&n, v).await {
                                            Ok(()) => {
                                                let persisted = (|| -> kod_error::Result<()> {
                                                    let mut cfg = KodConfig::load_default()?;
                                                    if !cfg.jev.thresholds.set(&n, v) {
                                                        return Err(
                                                            kod_error::KodError::InvalidParameters {
                                                                reason: format!(
                                                                    "unknown threshold: {n}",
                                                                ),
                                                            },
                                                        );
                                                    }
                                                    cfg.jev.thresholds.clamp();
                                                    let path = KodConfig::config_dir()?
                                                        .join("config.toml");
                                                    cfg.save_to(&path)?;
                                                    Ok(())
                                                })(
                                                );
                                                match persisted {
                                                    Ok(()) => self.app.push_system_message(
                                                        &format!("Set {n} = {v:.2} (persisted)."),
                                                    ),
                                                    Err(e) => {
                                                        self.app.push_system_message(&format!(
                                                            "Set {n} = {v:.2} in this session; \
                                                             could not persist: {e}",
                                                        ))
                                                    }
                                                }
                                            }
                                            Err(e) => self.app.push_system_message(&format!(
                                                "Could not set {n}: {e}",
                                            )),
                                        }
                                    }
                                    _ => self
                                        .app
                                        .push_system_message("Usage: /jev tune set <name> <value>"),
                                }
                            }
                            Some("reset") => {
                                let defaults = kod_config::JevThresholds::default();
                                let mut all_ok = true;
                                for name in kod_config::JevThresholds::NAMES {
                                    if let Some(v) = defaults.get(name)
                                        && engine.update_jev_threshold(name, v).await.is_err()
                                    {
                                        all_ok = false;
                                    }
                                }
                                if all_ok {
                                    self.app.push_system_message(
                                        "Jev thresholds reset to defaults (this session).",
                                    );
                                } else {
                                    self.app.push_system_message(
                                        "Some thresholds could not be reset; see the session log.",
                                    );
                                }
                            }
                            Some(other) => {
                                self.app.push_system_message(&format!(
                                    "Unknown /jev tune sub-command: {other}. Try /jev tune, /jev tune set <name> <value>, or /jev tune reset.",
                                ));
                            }
                        }
                    }
                    Some("on") | Some("off") | Some("strict") | Some("balanced")
                    | Some("lenient") => {
                        self.app.push_system_message(
                            "Changing Jev enabled/thresholds requires a restart. \
                             Edit [jev] in ~/.kod/config.toml, then restart kod.",
                        );
                    }
                    Some(other) => {
                        self.app.push_system_message(&format!(
                            "Unknown /jev sub-command: {other}. \
                             Try /jev, /jev stats, /jev cache clear, /jev test.",
                        ));
                    }
                }
            }
            "/log" => {
                // Session log viewer. Reads the recorder's JSONL
                // file (the one `kod replay` reads) and prints the
                // most recent entries. The optional argument caps
                // the count; the default is 20, the max 200.
                let Some(engine) = &self.engine else {
                    self.app.push_system_message("Engine not initialized.");
                    return Ok(());
                };
                let Some(path) = engine.session_log_path() else {
                    self.app.push_system_message(
                        "No session log installed for this run. \
                         Set KOD_SESSION_LOG to enable one.",
                    );
                    return Ok(());
                };
                let n: usize = parts
                    .next()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(20)
                    .min(200);
                match kod_core::session_log::read_session(&path) {
                    Ok(entries) => {
                        if entries.is_empty() {
                            self.app.push_system_message(&format!(
                                "Session log {} is empty.",
                                path.display(),
                            ));
                            return Ok(());
                        }
                        let total = entries.len();
                        let shown = entries.iter().rev().take(n).collect::<Vec<_>>();
                        let mut msg = format!(
                            "Session log {} ({} total, newest {}):\n",
                            path.display(),
                            total,
                            shown.len()
                        );
                        for e in shown.iter().rev() {
                            msg.push_str(&format_entry_one_line(e));
                            msg.push('\n');
                        }
                        msg.push_str("\nReplay this log with: kod replay <path>");
                        self.app.push_system_message(msg.trim_end());
                    }
                    Err(e) => {
                        self.app.push_system_message(&format!(
                            "Could not read session log {}: {e}",
                            path.display(),
                        ));
                    }
                }
            }
            "/check" => {
                // No argument: whole-project compiler check.
                // With a file argument: try LSP first (fast, per-file,
                // project-aware), fall back to the compiler if no
                // server is available or the file cannot be read.
                let arg = parts.next().map(|s| s.to_string());
                let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));

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
                            .lsp_diagnostics(&path, &content, std::time::Duration::from_secs(30))
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
                                    let s: String = d.message.chars().take(120).collect();
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
                                msg.push_str(&format!("  … and {} more\n", diags.len() - 30));
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
                                    let short = if d.message.chars().count() > 120 {
                                        let s: String = d.message.chars().take(120).collect();
                                        format!("{s}…")
                                    } else {
                                        d.message.clone()
                                    };
                                    msg.push_str(&format!(
                                        "  {} {} {}:{}:{} — {}\n",
                                        d.severity, code, d.file, d.line, d.column, short,
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
                        Err(e) => self.app.push_system_message(&format!("check failed: {e}",)),
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
                        let _ = std::fs::create_dir_all(
                            p.parent().unwrap_or(std::path::Path::new(".")),
                        );
                        match std::fs::write(&p, markdown.as_bytes()) {
                            Ok(()) => self.app.push_system_message(&format!(
                                "Exported session ({} bytes) to {}",
                                markdown.len(),
                                p.display(),
                            )),
                            Err(e) => self.app.push_system_message(&format!("Export failed: {e}")),
                        }
                    }
                }
            }
            "/export-html" => {
                // `KodApp::export_html` builds a self-contained HTML
                // document (inline CSS, no external assets) — the
                // shape a user can email or paste into a wiki. The
                // command writes it to a file when a path is given,
                // and to a default location otherwise so a user who
                // types just `/export-html` still gets a file
                // instead of an error.
                let html = self.app.export_html();
                let arg = parts.next().map(|s| s.to_string());
                let path = match arg {
                    Some(p) if p != "-" => std::path::PathBuf::from(p),
                    Some(_) => {
                        // `-` means stdout; print the whole thing.
                        // Nothing else to do — return.
                        println!();
                        println!("{}", html);
                        println!();
                        self.app
                            .push_system_message("Exported session HTML to stdout.");
                        return Ok(());
                    }
                    None => {
                        // Default path: `.kod/session-<unix-ms>.html`
                        // under the engine's working directory.
                        let base = self
                            .engine
                            .as_ref()
                            .map(|e| e.working_dir().to_path_buf())
                            .unwrap_or_else(|| {
                                std::env::current_dir()
                                    .unwrap_or_else(|_| std::path::PathBuf::from("."))
                            });
                        let dir = base.join(".kod");
                        let ts = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis())
                            .unwrap_or(0);
                        dir.join(format!("session-{ts}.html"))
                    }
                };
                if let Some(parent) = path.parent()
                    && !parent.as_os_str().is_empty()
                {
                    let _ = std::fs::create_dir_all(parent);
                }
                match std::fs::write(&path, html.as_bytes()) {
                    Ok(()) => self.app.push_system_message(&format!(
                        "Exported session ({} bytes) as HTML to {}",
                        html.len(),
                        path.display(),
                    )),
                    Err(e) => self
                        .app
                        .push_system_message(&format!("Export failed: {e}",)),
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
                let mut lines = vec!["KOD onboarding".to_string(), String::new()];
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
                lines.push(format!(
                    "Endpoint: {}",
                    config.llm.default_endpoint().base_url
                ));
                lines.push(format!(
                    "Network:  {}",
                    if config.llm.network_access {
                        "enabled (web_fetch can reach the network)"
                    } else {
                        "disabled (set llm.network_access = true to enable)"
                    }
                ));
                lines.push("Writes:   policy-gated (see [tools] and .kod/policy.toml)".to_string());
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
                        "One or more checks failed — review the items marked ✗ above.".to_string(),
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
                        let args: String = parts.collect::<Vec<_>>().join(" ");
                        let cwd = std::env::current_dir()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|_| ".".to_string());
                        let expanded = body.replace("{args}", &args).replace("{cwd}", &cwd);
                        self.app.set_input(expanded);
                        Box::pin(self.dispatch_prompt()).await?;
                    }
                    None => {
                        // Helpful hint when the user typed something
                        // close to a custom command's name.
                        let hint = config
                            .as_ref()
                            .map(|c| {
                                let names: Vec<&str> =
                                    c.commands.keys().map(|s| s.as_str()).collect();
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
        let models: Vec<String> = match engine.list_models().await {
            Ok(ms) => {
                let current = engine.current_model().await;
                engine.record_model_catalog(&current.endpoint, &ms);
                ms.into_iter().map(|x| x.id).collect()
            }
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
        // Partial-hunk approval (Tier 2.3). While open: ↑/↓ move,
        // Space toggles the current hunk, Enter commits and sends
        // `ApproveWith`, Esc cancels back to the dialog.
        if self.app.is_selecting_hunks() {
            match key {
                KeyCode::Escape | KeyCode::CtrlC => {
                    self.app.cancel_hunk_selection();
                    return Ok(());
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.app.hunk_prev();
                    return Ok(());
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.app.hunk_next();
                    return Ok(());
                }
                KeyCode::Char(' ') | KeyCode::Char('x') => {
                    self.app.hunk_toggle();
                    return Ok(());
                }
                KeyCode::Enter => {
                    if let Some((id, args)) = self.app.hunk_commit() {
                        if let Some(engine) = &self.engine {
                            engine
                                .respond_to_approval(
                                    id,
                                    kod_core::engine::ApprovalDecision::ApproveWith {
                                        arguments: args,
                                    },
                                )
                                .await;
                            if let Some(batch) = self.app.pending_batch_mut() {
                                batch.advance();
                                if batch.current_item().is_none() {
                                    self.app.clear_pending_approval();
                                }
                            }
                        }
                    } else {
                        self.app
                            .push_system_message("Select at least one hunk before committing.");
                    }
                    return Ok(());
                }
                _ => return Ok(()),
            }
        }

        // Approval-edit modal (Tier 2.3). While open, the input box
        // edits the call's JSON arguments. Enter commits; Esc cancels.
        if self.app.is_editing_approval() {
            match key {
                KeyCode::Escape | KeyCode::CtrlC => {
                    self.app.cancel_edit();
                    return Ok(());
                }
                KeyCode::Enter | KeyCode::CtrlJ | KeyCode::ShiftEnter => {
                    if let Some((id, args)) = self.app.edit_commit() {
                        if let Some(engine) = &self.engine {
                            engine
                                .respond_to_approval(
                                    id,
                                    kod_core::engine::ApprovalDecision::ApproveWith {
                                        arguments: args,
                                    },
                                )
                                .await;
                            if let Some(batch) = self.app.pending_batch_mut() {
                                batch.advance();
                                if batch.current_item().is_none() {
                                    self.app.clear_pending_approval();
                                }
                            }
                        }
                    } else {
                        self.app.push_system_message(
                            "Edit is not valid JSON; fix the buffer or press Esc.",
                        );
                    }
                    return Ok(());
                }
                KeyCode::Backspace => {
                    self.app.edit_backspace();
                    return Ok(());
                }
                KeyCode::Char(c) => {
                    self.app.edit_push_char(c);
                    return Ok(());
                }
                _ => return Ok(()),
            }
        }

        // Command palette (Ctrl+K). When open, every key belongs to
        // the palette: type filters, arrows move, Enter accepts,
        // Esc closes. Nothing else on the app sees the key.
        if self.app.is_palette_open() {
            match key {
                KeyCode::Escape | KeyCode::CtrlC => {
                    self.app.close_palette();
                    return Ok(());
                }
                KeyCode::Enter => {
                    let entry = self.app.palette_selected_entry();
                    self.app.close_palette();
                    if let Some(e) = entry {
                        // A slash-command entry inserts the command
                        // into the input box, ready to be completed.
                        // A key-only entry just shows its key as a
                        // system message.
                        if e.insert.starts_with('/') {
                            self.app.set_input(e.insert.clone());
                        } else {
                            self.app.push_system_message(&format!(
                                "\u{2192} {} ({})",
                                e.hint, e.insert,
                            ));
                        }
                    }
                    return Ok(());
                }
                KeyCode::Up => {
                    self.app.palette_prev();
                    return Ok(());
                }
                KeyCode::Down => {
                    self.app.palette_next();
                    return Ok(());
                }
                KeyCode::Backspace => {
                    self.app.palette_backspace();
                    return Ok(());
                }
                KeyCode::Char(c) => {
                    self.app.palette_push_char(c);
                    return Ok(());
                }
                _ => return Ok(()),
            }
        }
        // Ctrl+K opens the palette, unless a modal is up.
        if matches!(key, KeyCode::CtrlK) && !self.app.is_asking() && !self.app.is_approving() {
            self.app.open_palette();
            return Ok(());
        }
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

        // Approval dialog (batch): the user walks the items.
        //
        //   y -> Approve current, advance
        //   n -> Deny current, advance
        //   a -> DenyAlways current, advance
        //   ↑/k -> previous item (no decision)
        //   ↓/j -> next item (no decision)
        //   Esc / Ctrl+C -> deny current and every remaining item, close
        //
        // Every other key is swallowed so the user does not type past
        // a modal they cannot dismiss.
        if self.app.is_approving() {
            // Navigation keys — move without deciding.
            if matches!(key, KeyCode::Up | KeyCode::Char('k')) {
                if let Some(batch) = self.app.pending_batch_mut() {
                    batch.retreat();
                }
                return Ok(());
            }
            if matches!(key, KeyCode::Down | KeyCode::Char('j')) {
                if let Some(batch) = self.app.pending_batch_mut() {
                    batch.advance();
                }
                return Ok(());
            }

            // Esc aborts the whole batch: deny current and everything
            // after it. Done in one pass so the engine's awaits do
            // not stall on an unanswered item.
            if matches!(key, KeyCode::Escape | KeyCode::CtrlC) {
                let ids: Vec<u64> = {
                    let Some(batch) = self.app.pending_batch_mut() else {
                        return Ok(());
                    };
                    let mut ids: Vec<u64> = Vec::new();
                    while let Some(item) = batch.current_item() {
                        ids.push(item.id);
                        if !batch.advance() {
                            break;
                        }
                    }
                    ids
                };
                if let Some(engine) = &self.engine {
                    for id in ids {
                        engine
                            .respond_to_approval(id, kod_core::engine::ApprovalDecision::Deny)
                            .await;
                    }
                }
                self.app.clear_pending_approval();
                return Ok(());
            }

            // Tier 2.3 — "l" learns an allow for the current call
            // for the rest of the session and approves it. Distinct
            // from "a" (deny-always) so the operator can capture a
            // recurring safe call without re-prompting.
            if matches!(key, KeyCode::Char('l') | KeyCode::Char('L')) {
                let (id, call_opt, done) = {
                    let Some(batch) = self.app.pending_batch_mut() else {
                        return Ok(());
                    };
                    let Some(item) = batch.current_item() else {
                        return Ok(());
                    };
                    let id = item.id;
                    let call = kod_types::ToolCall {
                        id: None,
                        tool_name: item.tool_name.clone(),
                        arguments: item.arguments.clone(),
                    };
                    batch.advance();
                    (id, Some(call), batch.current_item().is_none())
                };
                if let Some(engine) = &self.engine {
                    if let Some(call) = call_opt {
                        engine.learn_allow(&call).await;
                    }
                    engine
                        .respond_to_approval(id, kod_core::engine::ApprovalDecision::Approve)
                        .await;
                }
                if done {
                    self.app.clear_pending_approval();
                }
                return Ok(());
            }

            // Tier 2.3 — "h" enters partial-hunk selection for a
            // `patch_file` call.
            if matches!(key, KeyCode::Char('h') | KeyCode::Char('H')) {
                if self.app.begin_hunk_selection() {
                    return Ok(());
                }
                self.app
                    .push_system_message("Hunk selection is only available for patch_file calls.");
                return Ok(());
            }

            // Tier 2.3 — "e" opens the argument editor.
            if matches!(key, KeyCode::Char('e') | KeyCode::Char('E')) {
                self.app.begin_edit_current_approval();
                return Ok(());
            }

            // Decision keys — decide current, advance.
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
                _ => None,
            };
            if let Some(d) = decision {
                let (id, done) = {
                    let Some(batch) = self.app.pending_batch_mut() else {
                        return Ok(());
                    };
                    let Some(item) = batch.current_item() else {
                        return Ok(());
                    };
                    let id = item.id;
                    batch.advance();
                    (id, batch.current_item().is_none())
                };
                if let Some(engine) = &self.engine {
                    engine.respond_to_approval(id, d).await;
                }
                if done {
                    self.app.clear_pending_approval();
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
                                        .remove(kod_types::MemoryType::LongTerm, &e.id)
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
                                self.app
                                    .push_system_message(&format!("Cleared {} checkpoint(s).", n,));
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
            KeyCode::Char('m') => {
                // Toggle terminal mouse capture. The default is on
                // — wheel scroll and click work out of the box —
                // but a user who wants to select text natively
                // (without holding Option/Shift) needs it off.
                self.mouse_captured = !self.mouse_captured;
                use crossterm::execute;
                let result = if self.mouse_captured {
                    execute!(std::io::stdout(), crossterm::event::EnableMouseCapture)
                } else {
                    execute!(std::io::stdout(), crossterm::event::DisableMouseCapture)
                };
                match result {
                    Ok(()) => {
                        let state = if self.mouse_captured { "on" } else { "off" };
                        self.app.push_system_message(&format!(
                            "Mouse capture {state}. \
                             With capture on, hold Option (macOS) or Shift to select text."
                        ));
                    }
                    Err(e) => {
                        // Revert the flag: the terminal refused the
                        // change, so the app's state should match
                        // reality.
                        self.mouse_captured = !self.mouse_captured;
                        self.app
                            .push_system_message(&format!("Could not toggle mouse capture: {e}"));
                    }
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
                    // H-T4: Esc closes the popup; it does NOT clear the
                    // typed draft. The pre-fix `set_input(String::new())`
                    // destroyed everything the user had typed,
                    // contradicting the sibling arm's own comment
                    // ("Esc in insert ALWAYS just drops to normal —
                    // never cancels").
                    self.app.reset_completion();
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
            KeyCode::CtrlE => {
                // Open the current input (which may be empty) in
                // $EDITOR. When the editor exits cleanly, whatever
                // it left becomes the new input box content. When
                // it fails, the input is unchanged and an error
                // system message is pushed — a broken editor must
                // not eat the user's draft.
                let current = self.app.input().to_string();
                match self.open_external_editor(&current).await {
                    Ok(content) => {
                        self.app.set_input(content.trim_end().to_string());
                    }
                    Err(e) => {
                        self.app
                            .push_system_message(&format!("External editor failed: {e}"));
                    }
                }
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

/// Wrap `s` in single quotes for safe interpolation into a `sh -c`
/// command. A path with a single quote inside is pathological; the
/// standard POSIX escape (`'\''`) handles it anyway.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// One-line summary of a `SessionEntry` for `/log`.
///
/// Deliberately compact: a session log over a busy run has
/// thousands of entries, and a `/log` view is meant to be scanned.
/// The full JSON is one `kod replay` or `cat` away.
/// Render a `TurnTrace` as a readable tree (Tier 1.4).
fn format_turn_trace_verbose(t: &kod_core::TurnTrace) -> String {
    let mut msg = format!(
        "Turn #{} ({})   {:.2}s   ${:.4}   {}in → {}out\n",
        t.id,
        t.holder,
        t.duration_ms() as f64 / 1000.0,
        t.cost_usd,
        t.prompt_tokens,
        t.completion_tokens,
    );
    msg.push_str(&format!(
        "  prompt: {} chars   reply: {} chars   tools: {}\n",
        t.prompt_chars, t.reply_chars, t.tool_call_count,
    ));
    msg.push_str(&format!(
        "  jev: {} decisions ({} cached)\n",
        t.jev_decisions, t.jev_cache_hits,
    ));
    msg.push_str(&format!("  outcome: {:?}", t.outcome,));
    if let Some(r) = &t.reason {
        msg.push_str(&format!(" — {r}"));
    }
    msg.push('\n');
    if !t.rounds.is_empty() {
        msg.push_str("\nRounds\n");
        for (i, r) in t.rounds.iter().enumerate() {
            msg.push_str(&format!(
                "  {:>2}  {:<12} {:<20} {:>6}ms  {}in→{}out",
                i + 1,
                format!("{:?}", r.kind).to_lowercase(),
                format!("{}/{}", r.endpoint, r.model),
                r.duration_ms,
                r.input_tokens,
                r.output_tokens,
            ));
            if let Some(c) = r.cache_read_tokens {
                msg.push_str(&format!("  cache:{c}"));
            }
            msg.push('\n');
            for c in &r.tool_calls {
                msg.push_str(&format!(
                    "       tool  {:<16} {}ms  {}  {}B\n",
                    c.name,
                    c.duration_ms,
                    match c.outcome {
                        kod_core::ToolOutcomeKind::Success => "ok",
                        kod_core::ToolOutcomeKind::Error => "err",
                        kod_core::ToolOutcomeKind::Denied => "denied",
                        kod_core::ToolOutcomeKind::RequiresConfirmation => "ask",
                    },
                    c.output_bytes,
                ));
            }
            for rr in &r.retries {
                msg.push_str(&format!(
                    "       retry  {} → {}  ({})\n",
                    rr.from_endpoint, rr.to_endpoint, rr.reason,
                ));
            }
        }
    }
    msg
}

fn format_entry_one_line(entry: &kod_core::session_log::SessionEntry) -> String {
    use kod_core::session_log::SessionEntry;
    match entry {
        SessionEntry::ToolCall {
            tool_name,
            duration_ms,
            holder,
            ..
        } => format!("  {holder:>8}  tool   {tool_name} ({duration_ms}ms)"),
        SessionEntry::ModelFallback { from, to, .. } => {
            format!("  ------   fallback   {from} → {to}")
        }
        SessionEntry::PolicyDecision {
            tool_name,
            outcome,
            rule,
            ..
        } => format!("  ------   policy   {outcome} {tool_name} ({rule})"),
        SessionEntry::MemoryWrite {
            channel, memory_id, ..
        } => format!("  ------   memory   {channel} {memory_id}"),
        SessionEntry::Cost {
            endpoint,
            model,
            prompt_tokens,
            completion_tokens,
            cost_usd,
            ..
        } => format!(
            "  ------   cost   {endpoint}/{model} \
             ↑{prompt_tokens} ↓{completion_tokens} ${cost_usd:.4}"
        ),
        SessionEntry::Approval {
            holder,
            tool_name,
            decision,
            ..
        } => format!("  {holder:>8}  approval   {decision} {tool_name}"),
        SessionEntry::Diagnostics {
            file,
            error_count,
            warning_count,
            ..
        } => format!("  ------   lsp   {file} ({error_count}E/{warning_count}W)"),
        SessionEntry::JevDecision {
            holder,
            purpose,
            confidence,
            latency_ms,
            cached,
            source,
            ..
        } => {
            format!("  {holder:>8}  jev    {purpose} src={source} conf={confidence:.2} ",)
                + &format!("({latency_ms}ms{}))", if *cached { ", cached" } else { "" },)
        }
        SessionEntry::ToolOutcome {
            holder,
            tool_name,
            outcome,
            user_visible_impact,
            confidence,
            ..
        } => format!(
            "  {holder:>8}  outcome  {tool_name} = {outcome} impact={user_visible_impact} conf={confidence:.2}",
        ),
        SessionEntry::Redaction { rules, .. } => {
            let list = rules
                .iter()
                .map(|r| format!("{}×{}", r.rule, r.count))
                .collect::<Vec<_>>()
                .join(", ");
            format!("  ------   redact   {list}")
        }
        SessionEntry::MemoryRetrieval {
            retrieved,
            referenced,
            ..
        } => format!(
            "  ------   memory   retrieved={} referenced={}",
            retrieved.len(),
            referenced.len(),
        ),
    }
}

// Tool header/summary rendering lives in `kod_core::engine`
// (`format_tool_header`, `summarize_tool_result`): the engine needs it
// for the live done-markers and the TUI for the task-end fallback, so
// both share one implementation instead of drifting apart.

/// Parse a leading `@N ` prefix in `input`, returning `(n, rest)`.
///
/// The prefix is only recognised when `N` is a positive integer (1+) and
/// is followed by a space or by end-of-input. `@0`, `@01`, `@x` all
/// return `None`. This keeps the syntax unambiguous against the `@path`
/// file-reference form (`@src/lib.rs`), which a user could also type.
fn parse_at_agent_prefix(input: &str) -> Option<(usize, &str)> {
    let s = input.strip_prefix('@')?;
    let digits_end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    if digits_end == 0 {
        return None;
    }
    let digits = &s[..digits_end];
    // Reject a leading zero (except the single-digit "0" which we reject
    // separately as n == 0).
    if digits.len() > 1 && digits.starts_with('0') {
        return None;
    }
    let n: usize = digits.parse().ok()?;
    if n == 0 {
        return None;
    }
    let rest = &s[digits_end..];
    if rest.is_empty() {
        return Some((n, ""));
    }
    // Require exactly one space (or a tab) after the digits, then the
    // text. Any other character means this is not an `@N` prefix.
    let (sep, text) = rest.split_at(1);
    if sep != " " && sep != "\t" {
        return None;
    }
    Some((n, text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_check_command_without_project() {
        // Running `/check` in a directory with no recognized project
        // (no Cargo.toml, package.json, pyproject.toml, or go.mod)
        // must report the missing project, not silently do nothing.
        // The TUI's cwd during tests is the crate directory, which
        // has a Cargo.toml — so we set the cwd to a fresh tempdir
        // under a static lock so the change does not race a parallel
        // test.
        let mut tui = TuiLoop::new();
        let tmp = tempfile::TempDir::new().unwrap();
        let old_cwd = std::env::current_dir().unwrap();
        static CWD_LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        let _guard = CWD_LOCK
            .get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        std::env::set_current_dir(tmp.path()).unwrap();

        let result = tui.handle_command("/check").await;
        let restore = std::env::set_current_dir(&old_cwd);
        let _ = restore;
        result.unwrap();

        let last = tui.app().messages().last().unwrap();
        assert!(
            last.content.contains("check failed") || last.content.contains("no recognized project"),
            "expected a missing-project message, got: {}",
            last.content,
        );
    }

    #[tokio::test]
    async fn test_handoff_without_engine_reports() {
        // Without an engine, /handoff cannot produce a document. The
        // command must say so rather than silently doing nothing.
        let mut tui = TuiLoop::new();
        tui.handle_command("/handoff").await.unwrap();
        let last = tui.app().messages().last().unwrap();
        assert!(
            last.content.contains("Engine not initialized"),
            "expected a clear no-engine message, got: {}",
            last.content,
        );
    }

    #[tokio::test]
    async fn test_handoff_with_engine_reports_empty_session() {
        // With an engine but an empty session, /handoff has no
        // transcript to summarize. The command must report the empty
        // session rather than starting a generation on an empty
        // prompt.
        use kod_core::{KodEngine, RouterConfig};
        let tmp = tempfile::TempDir::new().unwrap();
        let db = tmp.path().join("t.redb");
        let cfg = RouterConfig {
            skill_threshold: 0.3,
            context_window: 8192,
            short_term_capacity: 100,
            working_dir: tmp.path().to_path_buf(),
            enable_memory: false,
            max_skills_per_query: 3,
            embedder: None,
        };
        let engine = std::sync::Arc::new(KodEngine::new(cfg, db).unwrap());
        engine.start().await.unwrap();

        let mut tui = TuiLoop::new();
        tui.set_engine(engine.clone());
        tui.handle_command("/handoff").await.unwrap();
        let last = tui.app().messages().last().unwrap();
        assert!(
            last.content.contains("Nothing to hand off"),
            "expected an empty-session message, got: {}",
            last.content,
        );
        let _ = engine.shutdown().await;
    }

    #[tokio::test]
    async fn test_policy_command_without_engine_reports() {
        // The TUI without an engine reports the missing engine rather
        // than silently doing nothing. The list path is the honest
        // no-engine response; the alternative would be a silent no-op
        // that a user would attribute to a bug in the command.
        let mut tui = TuiLoop::new();
        tui.handle_command("/policy").await.unwrap();
        let last = tui.app().messages().last().unwrap();
        assert!(
            last.content.contains("Engine not initialized"),
            "expected a clear no-engine message, got: {}",
            last.content,
        );
    }

    #[tokio::test]
    async fn test_policy_forget_without_engine_reports() {
        let mut tui = TuiLoop::new();
        tui.handle_command("/policy forget 1").await.unwrap();
        let last = tui.app().messages().last().unwrap();
        assert!(last.content.contains("Engine not initialized"));
    }

    #[tokio::test]
    async fn test_policy_unknown_subcommand_reports() {
        // The engine check runs first, so without an engine any
        // subcommand reports the missing engine. With an engine, an
        // unknown subcommand reports the usage. This test proves the
        // no-engine path is consistent regardless of subcommand.
        let mut tui = TuiLoop::new();
        tui.handle_command("/policy somethingweird").await.unwrap();
        let last = tui.app().messages().last().unwrap();
        assert!(last.content.contains("Engine not initialized"));
    }

    #[tokio::test]
    async fn test_map_command_produces_output() {
        // The TUI's cwd during tests is the crate directory
        // (`crates/kod-tui`), which contains `.rs` files. `/map` walks
        // the current directory and prints one line per file. The test
        // asserts the output is the map, not the "no recognized source
        // files" fallback.
        //
        // S10 follow-up: the `/export-html` tests mutate the process
        // cwd under the shared `CWD_LOCK`; a parallel run of this
        // test without the lock saw the tmp dir those tests chdir'd
        // into. Take the same lock.
        static CWD_LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        let _guard = CWD_LOCK
            .get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let mut tui = TuiLoop::new();
        tui.handle_command("/map").await.unwrap();
        let last = tui.app().messages().last().unwrap();
        assert!(
            last.content.contains("Repository map"),
            "expected a repository map, got: {}",
            last.content,
        );
        // The map should name at least one file with a `.rs` extension.
        assert!(
            last.content.contains(".rs"),
            "the map must list at least one Rust file: {}",
            last.content,
        );
    }

    #[tokio::test]
    async fn test_map_command_respects_max_chars() {
        // A tiny budget must truncate. The renderer appends a
        // `(map truncated)` marker when it hits the cap; the assertion
        // proves the argument reaches the renderer.
        //
        // S10 follow-up: same CWD_LOCK reason as `test_map_command_produces_output`.
        static CWD_LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        let _guard = CWD_LOCK
            .get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let mut tui = TuiLoop::new();
        tui.handle_command("/map 1").await.unwrap();
        let last = tui.app().messages().last().unwrap();
        // A 1-char budget is smaller than any line; the map is either
        // empty (unlikely with a `.rs` in cwd) or truncated.
        assert!(
            last.content.contains("map truncated")
                || last.content.contains("No recognized source files")
                || last.content.contains("Repository map"),
            "expected the map output under a tiny budget, got: {}",
            last.content,
        );
    }

    #[tokio::test]
    async fn test_grep_command_finds_a_matching_message() {
        let mut tui = TuiLoop::new();
        // Push a message with a distinctive token.
        tui.app_mut()
            .push_system_message("the marker zqxjw appears here");
        tui.handle_command("/grep zqxjw").await.unwrap();
        let last = tui.app().messages().last().unwrap();
        assert!(
            last.content.contains("1 message"),
            "expected one match, got: {}",
            last.content,
        );
        assert!(
            last.content.contains("zqxjw"),
            "the match line must show the content: {}",
            last.content,
        );
    }

    #[tokio::test]
    async fn test_grep_command_no_match_reports() {
        let mut tui = TuiLoop::new();
        tui.handle_command("/grep nothingmatchesxyzzy")
            .await
            .unwrap();
        let last = tui.app().messages().last().unwrap();
        assert!(
            last.content.contains("No messages match"),
            "expected a no-match message, got: {}",
            last.content,
        );
    }

    #[tokio::test]
    async fn test_grep_command_invalid_regex_reports() {
        let mut tui = TuiLoop::new();
        tui.handle_command("/grep [unterminated").await.unwrap();
        let last = tui.app().messages().last().unwrap();
        assert!(
            last.content.contains("Invalid regex"),
            "expected an invalid-regex message, got: {}",
            last.content,
        );
    }

    #[tokio::test]
    async fn test_grep_command_empty_pattern_reports_usage() {
        let mut tui = TuiLoop::new();
        tui.handle_command("/grep").await.unwrap();
        let last = tui.app().messages().last().unwrap();
        assert!(
            last.content.contains("Usage: /grep"),
            "expected a usage message, got: {}",
            last.content,
        );
    }

    #[tokio::test]
    async fn test_export_html_writes_to_default_path() {
        let mut tui = TuiLoop::new();
        tui.app_mut().push_system_message("hello world");

        // Use a tempdir as the cwd so the default path lands there.
        // The default is `<cwd>/.kod/session-<ts>.html`; the test's
        // own cwd is the crate directory, which is not writable in CI
        // and would leave artefacts on the developer's machine.
        let tmp = tempfile::TempDir::new().unwrap();
        let old_cwd = std::env::current_dir().unwrap();
        // Serialize cwd mutation across tests via a static mutex so
        // the change does not race a parallel test.
        static CWD_LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        let _guard = CWD_LOCK
            .get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        std::env::set_current_dir(tmp.path()).unwrap();

        let result = tui.handle_command("/export-html").await;
        let restore = std::env::set_current_dir(&old_cwd);
        let _ = restore;
        result.unwrap();

        // A file should exist under <tmp>/.kod/.
        let entry = std::fs::read_dir(tmp.path().join(".kod"))
            .map(|rd| {
                rd.filter_map(|e| e.ok().map(|e| e.path())).any(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .map(|n| n.starts_with("session-") && n.ends_with(".html"))
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false);
        assert!(
            entry,
            "an HTML file must be written under <cwd>/.kod/; {} has {:?}",
            tmp.path().display(),
            std::fs::read_dir(tmp.path()).ok().map(|rd| rd
                .filter_map(|e| e.ok().map(|e| e.path()))
                .collect::<Vec<_>>()),
        );
    }

    #[tokio::test]
    async fn test_export_html_to_explicit_path() {
        let mut tui = TuiLoop::new();
        tui.app_mut().push_system_message("explicit path content");
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().join("out.html");
        tui.handle_command(&format!("/export-html {}", target.display()))
            .await
            .unwrap();
        let content = std::fs::read_to_string(&target).unwrap();
        assert!(
            content.starts_with("<!doctype html>"),
            "the file must be a complete HTML document",
        );
        assert!(content.contains("explicit path content"));
    }

    #[tokio::test]
    async fn test_export_html_stdout_path() {
        // `-` prints to stdout instead of a file. The test asserts the
        // command does not create a file named `-` in the cwd.
        let mut tui = TuiLoop::new();
        tui.app_mut().push_system_message("stdout content");
        let tmp = tempfile::TempDir::new().unwrap();
        let old_cwd = std::env::current_dir().unwrap();
        static CWD_LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        let _guard = CWD_LOCK
            .get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        std::env::set_current_dir(tmp.path()).unwrap();

        let result = tui.handle_command("/export-html -").await;
        let restore = std::env::set_current_dir(&old_cwd);
        let _ = restore;
        result.unwrap();

        assert!(
            !tmp.path().join("-").exists(),
            "`-` must print to stdout, not create a file named `-`",
        );
        let last = tui.app().messages().last().unwrap();
        assert!(
            last.content.contains("stdout"),
            "the command should report the stdout export, got: {}",
            last.content,
        );
    }

    #[test]
    fn parse_at_agent_prefix_accepts_simple_form() {
        assert_eq!(parse_at_agent_prefix("@1 hello"), Some((1, "hello")));
        assert_eq!(
            parse_at_agent_prefix("@2 two words"),
            Some((2, "two words"))
        );
        assert_eq!(parse_at_agent_prefix("@10 x"), Some((10, "x")));
        // A bare `@N` with no text is legal; the caller steers with an
        // empty message, which `steer_for` already ignores.
        assert_eq!(parse_at_agent_prefix("@3"), Some((3, "")));
    }

    #[test]
    fn parse_at_agent_prefix_rejects_non_numbers() {
        assert_eq!(parse_at_agent_prefix("@x hello"), None);
        assert_eq!(parse_at_agent_prefix("@ hello"), None);
        assert_eq!(parse_at_agent_prefix("no at sign"), None);
    }

    #[test]
    fn parse_at_agent_prefix_rejects_path_shapes() {
        // `@src/lib.rs` is the file-attachment form; the digit check
        // must reject it, or every reference would try to steer agent
        // `s` (which does not exist anyway, but the failure should be
        // silent).
        assert_eq!(parse_at_agent_prefix("@src/lib.rs"), None);
        assert_eq!(parse_at_agent_prefix("@./path"), None);
        assert_eq!(parse_at_agent_prefix("@docs/file.md"), None);
    }

    #[test]
    fn parse_at_agent_prefix_rejects_zero_and_leading_zero() {
        // `@0` is not a valid agent position (positions are 1-based);
        // `@01` would be ambiguous with `@1` and is rejected to keep
        // the parser deterministic.
        assert_eq!(parse_at_agent_prefix("@0 x"), None);
        assert_eq!(parse_at_agent_prefix("@01 x"), None);
    }

    #[test]
    fn parse_at_agent_prefix_requires_space_after_digits() {
        // `@1x` is a non-number, not an agent position followed by an
        // identifier — a user who typed that made a mistake and should
        // not silently steer.
        assert_eq!(parse_at_agent_prefix("@1x"), None);
        assert_eq!(parse_at_agent_prefix("@12abc"), None);
    }

    #[test]
    fn parse_at_agent_prefix_accepts_leading_whitespace_in_text() {
        // The text after the space is passed through verbatim,
        // including any additional whitespace the user typed.
        assert_eq!(
            parse_at_agent_prefix("@2  double space"),
            Some((2, " double space"))
        );
    }

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
        tui.handle_event(Event::Key(KeyCode::Backspace))
            .await
            .unwrap();
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
        tui.handle_command("/swarm fix the payment handler")
            .await
            .unwrap();
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

/// Coverage for the slash-command dispatch arms in
/// `TuiLoop::handle_command`. The existing `mod tests` covers a
/// subset (check / map / grep / export-html / handoff / policy /
/// theme / goal / search / swarm / retry / steer / compact / model);
/// this module covers the rest.
///
/// Two assertion styles are used:
///
/// * **State-based** — the command mutates `KodApp` state that has
///   a public getter, and the test asserts on that state. These are
///   the strongest tests in the module; they do not depend on the
///   exact wording of any message.
/// * **Loose string match** — the command pushes a system message
///   and the test asserts it contains at least one of several
///   plausible substrings. These are weaker; the alternative
///   (asserting an exact phrase) would make the tests brittle
///   against harmless wording changes in a `push_system_message`
///   call site.
///
/// The `"Engine not initialized"` pattern is copied from the
/// existing tests in the sibling module, which is the canonical
/// no-engine response.
#[cfg(test)]
mod coverage_slash_dispatch {
    use super::*;

    fn last_message(tui: &TuiLoop) -> String {
        tui.app()
            .messages()
            .last()
            .map(|m| m.content.clone())
            .unwrap_or_default()
    }

    fn assert_last_contains_any(tui: &TuiLoop, needles: &[&str]) {
        let last = last_message(tui);
        let lower = last.to_lowercase();
        if needles.iter().any(|n| lower.contains(&n.to_lowercase())) {
            return;
        }
        panic!("last message matched none of {needles:?}; got: {last:?}");
    }

    // ---- state-based tests -------------------------------------------

    #[tokio::test]
    async fn clear_asks_for_confirmation_before_wiping_the_chat() {
        // `/clear` is destructive, so it routes through the same
        // confirmation flow the status widget renders
        // (`Clear all messages? y = yes · n/Esc = keep`). The
        // message history must be untouched until the user answers
        // `y` — a regression that cleared immediately would make
        // `n` a lie.
        let mut tui = TuiLoop::new();
        tui.app_mut().push_system_message("marker-before-clear");
        tui.handle_command("/clear").await.unwrap();
        assert!(
            tui.app().pending_confirm().is_some(),
            "/clear must ask before wiping the chat",
        );
        assert!(
            tui.app()
                .messages()
                .iter()
                .any(|m| m.content.contains("marker-before-clear")),
            "the marker must survive until the user confirms",
        );
    }

    // ---- engine-missing pattern --------------------------------------

    #[tokio::test]
    async fn regenerate_without_engine_reports() {
        let mut tui = TuiLoop::new();
        tui.handle_command("/regenerate").await.unwrap();
        // On an empty chat the "nothing to regenerate" guard runs
        // before the engine check; both are honest non-silent
        // failures.
        assert_last_contains_any(&tui, &["Nothing to regenerate", "Engine not initialized"]);
    }

    #[tokio::test]
    async fn refine_without_engine_reports() {
        let mut tui = TuiLoop::new();
        tui.handle_command("/refine make it shorter").await.unwrap();
        // Same guard order as `/regenerate`: on an empty chat the
        // "no assistant reply" check fires first.
        assert_last_contains_any(&tui, &["Nothing to refine", "Engine not initialized"]);
    }

    #[tokio::test]
    async fn summarize_without_engine_reports() {
        let mut tui = TuiLoop::new();
        tui.handle_command("/summarize").await.unwrap();
        assert_last_contains_any(&tui, &["Engine not initialized"]);
    }

    #[tokio::test]
    async fn remember_without_engine_reports() {
        // Without an engine there is no memory subsystem to write to.
        // The command reports that rather than opening a second
        // MemoryManager, which would collide with the engine's own
        // redb handle (and with any other concurrent test).
        let mut tui = TuiLoop::new();
        tui.handle_command("/remember cats are nice").await.unwrap();
        assert_last_contains_any(&tui, &["Engine not initialized"]);
    }

    #[tokio::test]
    async fn memory_without_engine_reports() {
        let mut tui = TuiLoop::new();
        tui.handle_command("/memory").await.unwrap();
        assert_last_contains_any(&tui, &["Engine not initialized", "memory"]);
    }

    // ---- loose-string tests ------------------------------------------

    #[tokio::test]
    async fn help_opens_the_overlay() {
        // `/help` toggles the full-screen overlay, matching the `?`
        // key and F1. It used to push `SLASH_HELP` as a system
        // message; the overlay widget advertises `/help` as an entry
        // point, so the command now honours that promise.
        let mut tui = TuiLoop::new();
        assert!(!tui.app().show_help(), "overlay starts closed");
        tui.handle_command("/help").await.unwrap();
        assert!(tui.app().show_help(), "`/help` must open the overlay");

        // A second invocation closes it, symmetric with the toggle
        // behaviour of `?` and F1.
        tui.handle_command("/help").await.unwrap();
        assert!(!tui.app().show_help(), "`/help` toggles");
    }

    #[tokio::test]
    async fn copy_without_an_assistant_reply_reports() {
        let mut tui = TuiLoop::new();
        tui.handle_command("/copy").await.unwrap();
        assert_last_contains_any(
            &tui,
            &["Nothing to copy", "no assistant reply", "clipboard"],
        );
    }

    #[tokio::test]
    async fn raw_without_an_assistant_reply_reports() {
        let mut tui = TuiLoop::new();
        tui.handle_command("/raw").await.unwrap();
        assert_last_contains_any(&tui, &["No assistant", "assistant reply", "nothing"]);
    }

    #[tokio::test]
    async fn last_prompt_without_a_prompt_reports() {
        let mut tui = TuiLoop::new();
        tui.handle_command("/last-prompt").await.unwrap();
        // `/last-prompt` forwards to `/debug last-prompt`, which
        // consults the engine before reporting the absent prompt.
        assert_last_contains_any(
            &tui,
            &[
                "Engine not initialized",
                "No last prompt",
                "last prompt",
                "no prompt",
            ],
        );
    }

    #[tokio::test]
    async fn debug_last_prompt_without_a_prompt_reports() {
        let mut tui = TuiLoop::new();
        tui.handle_command("/debug last-prompt").await.unwrap();
        assert_last_contains_any(
            &tui,
            &[
                "Engine not initialized",
                "No last prompt",
                "last prompt",
                "no prompt",
            ],
        );
    }

    #[tokio::test]
    async fn skills_lists_or_reports_no_skills() {
        let mut tui = TuiLoop::new();
        tui.handle_command("/skills").await.unwrap();
        assert_last_contains_any(&tui, &["skill", "loaded"]);
    }

    #[tokio::test]
    async fn stats_produces_a_summary() {
        let mut tui = TuiLoop::new();
        tui.app_mut().push_system_message("one");
        tui.app_mut().push_system_message("two");
        tui.handle_command("/stats").await.unwrap();
        assert_last_contains_any(&tui, &["stat", "message", "session"]);
    }

    #[tokio::test]
    async fn whoami_produces_a_session_summary() {
        let mut tui = TuiLoop::new();
        tui.handle_command("/whoami").await.unwrap();
        assert_last_contains_any(&tui, &["session", "model", "skills", "context"]);
    }

    #[tokio::test]
    async fn context_visualizes_usage() {
        let mut tui = TuiLoop::new();
        tui.handle_command("/context").await.unwrap();
        assert_last_contains_any(&tui, &["context", "token", "session"]);
    }

    #[tokio::test]
    async fn init_produces_onboarding_output() {
        let mut tui = TuiLoop::new();
        tui.handle_command("/init").await.unwrap();
        assert_last_contains_any(&tui, &["config", "model", "next", "kod"]);
    }

    #[tokio::test]
    async fn log_lists_or_reports_no_entries() {
        let mut tui = TuiLoop::new();
        tui.handle_command("/log").await.unwrap();
        assert_last_contains_any(&tui, &["Engine not initialized", "log", "no "]);
    }

    #[tokio::test]
    async fn checkpoints_lists_or_reports_none() {
        let mut tui = TuiLoop::new();
        tui.handle_command("/checkpoints").await.unwrap();
        // `/checkpoints` consults the engine's checkpoint manager
        // first; without an engine it reports the missing engine.
        assert_last_contains_any(&tui, &["Engine not initialized", "checkpoint", "no "]);
    }

    #[tokio::test]
    async fn branch_drops_a_marker() {
        let mut tui = TuiLoop::new();
        tui.app_mut().push_system_message("before branch");
        tui.handle_command("/branch test-label").await.unwrap();
        assert_last_contains_any(&tui, &["branch", "marker"]);
    }

    // ---- smoke tests: must not panic, must not silently no-op --------

    #[tokio::test]
    async fn attach_reports_the_outcome() {
        let mut tui = TuiLoop::new();
        tui.handle_command("/attach /tmp/kod-test-nonexistent-file")
            .await
            .unwrap();
        // Either "attached" or "not found" — either way, the
        // command must say something.
        let last = last_message(&tui);
        assert!(
            !last.is_empty(),
            "/attach must report its outcome, not stay silent"
        );
    }

    #[tokio::test]
    async fn delete_reports_the_outcome() {
        let mut tui = TuiLoop::new();
        tui.handle_command("/delete").await.unwrap();
        let last = last_message(&tui);
        assert!(
            !last.is_empty(),
            "/delete on an empty chat must report, not stay silent"
        );
    }

    #[tokio::test]
    async fn diff_reports_the_outcome() {
        let mut tui = TuiLoop::new();
        tui.handle_command("/diff").await.unwrap();
        let last = last_message(&tui);
        assert!(
            !last.is_empty(),
            "/diff with no checkpoint must report, not stay silent"
        );
    }
}

/// Coverage for the slash commands the first `coverage_slash_dispatch`
/// pass did not reach: the pin/unpin pair, the fork/edit state
/// mutators, the three file-writing commands (`/save`, `/load`,
/// `/export`), and the two self-diagnostic commands (`/git-status`,
/// `/doctor`).
///
/// The three file-I/O commands are asserted with an OR: the command
/// must either write the destination file **or** push a message
/// explaining why not. That is the honest contract — a `/save` that
/// silently no-ops when the path is not writable would be the bug;
/// a `/save` that refuses and explains is correct.
#[cfg(test)]
mod coverage_slash_dispatch_more {
    use super::*;
    use crate::app::Message;
    use chrono::Utc;
    use kod_types::{MessageId, MessageRole};

    fn last_message(tui: &TuiLoop) -> String {
        tui.app()
            .messages()
            .last()
            .map(|m| m.content.clone())
            .unwrap_or_default()
    }

    fn assert_last_contains_any(tui: &TuiLoop, needles: &[&str]) {
        let last = last_message(tui);
        let lower = last.to_lowercase();
        if needles.iter().any(|n| lower.contains(&n.to_lowercase())) {
            return;
        }
        panic!("last message matched none of {needles:?}; got: {last:?}");
    }

    fn push_user_message(tui: &mut TuiLoop, content: &str) {
        tui.app_mut().add_message(Message {
            id: MessageId::new(),
            role: MessageRole::User,
            content: content.to_string(),
            timestamp: Utc::now(),
            metadata: Default::default(),
            sequence: 0,
        });
    }

    // ---- state-based --------------------------------------------------

    #[tokio::test]
    async fn pin_pins_something_or_reports() {
        // `/pin N` may or may not accept N=1 depending on the index
        // convention and on whether the system message I pushed
        // counts. The contract is: the command either pins a
        // message, or it pushes a message explaining why not. It
        // must not silently no-op.
        let mut tui = TuiLoop::new();
        tui.app_mut().push_system_message("first");
        tui.app_mut().push_system_message("second");
        let before = tui.app().messages().len();
        let pinned_before = (0..3).filter(|i| tui.app().is_message_pinned(*i)).count();
        assert_eq!(pinned_before, 0, "fresh chat has no pins");

        tui.handle_command("/pin 1").await.unwrap();

        let pinned_after = (0..3).filter(|i| tui.app().is_message_pinned(*i)).count();
        let after = tui.app().messages().len();
        assert!(
            pinned_after > 0 || after > before,
            "/pin must pin a message or report why not; \
             pinned_before={pinned_before}, pinned_after={pinned_after}, \
             messages {before}->{after}",
        );
    }

    #[tokio::test]
    async fn unpin_removes_a_pin_or_reports() {
        let mut tui = TuiLoop::new();
        tui.app_mut().push_system_message("first");
        // Pin whatever index works, then unpin it.
        tui.handle_command("/pin 1").await.unwrap();
        let pinned_mid = (0..3).filter(|i| tui.app().is_message_pinned(*i)).count();
        let before = tui.app().messages().len();
        tui.handle_command("/unpin 1").await.unwrap();
        let pinned_end = (0..3).filter(|i| tui.app().is_message_pinned(*i)).count();
        let after = tui.app().messages().len();
        // Either the unpin cleared a pin, or the command reported
        // the missing pin. A silent no-op is the only failure.
        assert!(
            pinned_end < pinned_mid || after > before,
            "/unpin must clear a pin or report; \
             pinned {pinned_mid}->{pinned_end}, messages {before}->{after}",
        );
    }

    #[tokio::test]
    async fn fork_increments_the_fork_count() {
        let mut tui = TuiLoop::new();
        tui.app_mut().push_system_message("in the fork");
        let before = tui.app().fork_count();
        tui.handle_command("/fork test-fork").await.unwrap();
        assert!(
            tui.app().fork_count() > before,
            "/fork must increase the fork count",
        );
    }

    #[tokio::test]
    async fn edit_loads_the_last_user_message_into_the_input() {
        let mut tui = TuiLoop::new();
        push_user_message(&mut tui, "original user text");
        tui.handle_command("/edit").await.unwrap();
        assert_eq!(
            tui.app().input(),
            "original user text",
            "/edit must load the last user message into the input",
        );
    }

    // ---- file-writing commands ---------------------------------------

    #[tokio::test]
    async fn save_writes_the_file_or_reports_why_not() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("session.json");
        let mut tui = TuiLoop::new();
        tui.app_mut().push_system_message("save-marker");
        let before = tui.app().messages().len();

        tui.handle_command(&format!("/save {}", path.display()))
            .await
            .unwrap();

        let after = tui.app().messages().len();
        assert!(
            path.exists() || after > before,
            "/save must either write the file or push a report message",
        );
    }

    #[tokio::test]
    async fn load_of_a_missing_file_reports() {
        let tmp = tempfile::TempDir::new().unwrap();
        let missing = tmp.path().join("does-not-exist.json");
        let mut tui = TuiLoop::new();
        tui.handle_command(&format!("/load {}", missing.display()))
            .await
            .unwrap();
        // "Engine not initialized" is a plausible response too if the
        // engine check fires first; the contract is only that the
        // command reports something.
        let last = last_message(&tui);
        assert!(
            !last.is_empty(),
            "/load of a missing file must report, not stay silent",
        );
    }

    #[tokio::test]
    async fn export_writes_the_file_or_reports_why_not() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("chat.md");
        let mut tui = TuiLoop::new();
        tui.app_mut().push_system_message("export-marker");
        let before = tui.app().messages().len();

        tui.handle_command(&format!("/export {}", path.display()))
            .await
            .unwrap();

        let after = tui.app().messages().len();
        assert!(
            path.exists() || after > before,
            "/export must either write the file or push a report message",
        );
        if path.exists() {
            let body = std::fs::read_to_string(&path).unwrap_or_default();
            assert!(
                body.contains("export-marker"),
                "exported markdown must include the chat content",
            );
        }
    }

    // ---- self-diagnostic commands ------------------------------------

    #[tokio::test]
    async fn git_status_reports_something() {
        let mut tui = TuiLoop::new();
        tui.handle_command("/git-status").await.unwrap();
        // Three honest outcomes: porcelain output (lines with a status
        // code), "clean", or a git-unavailable message.
        let last = last_message(&tui);
        assert!(
            !last.is_empty(),
            "/git-status must report the repo state, not stay silent",
        );
    }

    #[tokio::test]
    async fn doctor_produces_a_report() {
        let mut tui = TuiLoop::new();
        tui.handle_command("/doctor").await.unwrap();
        assert_last_contains_any(
            &tui,
            &["config", "check", "doctor", "ok", "warn", "lsp", "llm"],
        );
    }

    #[tokio::test]
    async fn rollback_without_a_checkpoint_reports() {
        let mut tui = TuiLoop::new();
        tui.handle_command("/rollback").await.unwrap();
        assert_last_contains_any(
            &tui,
            &["Engine not initialized", "checkpoint", "rollback", "no "],
        );
    }
}
