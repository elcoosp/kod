//! Main event loop and UI rendering for the TUI.

use crate::app::{InputMode, KodApp};
use crate::event::Event;
use ratatui::backend::TestBackend;
use ratatui::prelude::*;
use ratatui::Terminal;
use ratatui::widgets::Paragraph;
use std::time::Duration;

/// Main TUI application loop
pub struct TuiLoop<B: Backend> {
    app: KodApp,
    should_quit: bool,
    _tick_rate: Duration,
    terminal: Option<Terminal<B>>,
}

impl TuiLoop<TestBackend> {
    /// Create a new TuiLoop (for testing without a real terminal)
    pub fn new() -> Self {
        Self {
            app: KodApp::new(),
            should_quit: false,
            _tick_rate: Duration::from_millis(250),
            terminal: None,
        }
    }
}

impl<B: Backend> TuiLoop<B> {
    /// Create a TuiLoop with a terminal
    pub fn with_terminal(terminal: Terminal<B>) -> Self {
        Self {
            app: KodApp::new(),
            should_quit: false,
            _tick_rate: Duration::from_millis(250),
            terminal: Some(terminal),
        }
    }

    pub fn app(&self) -> &KodApp {
        &self.app
    }

    pub fn app_mut(&mut self) -> &mut KodApp {
        &mut self.app
    }

    pub fn should_quit(&self) -> bool {
        self.should_quit
    }

    pub fn is_running(&self) -> bool {
        !self.should_quit
    }

    /// Not used in tests; requires a real terminal backend.
    pub fn init_terminal(&mut self) -> crate::app::Result<()> {
        // This method is only meaningful for concrete CrosstermBackend terminals.
        // For generic backends, use `with_terminal` instead.
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "init_terminal requires a concrete CrosstermBackend",
        ))
    }

    pub fn render(&mut self) -> crate::app::Result<()> {
        if let Some(terminal) = &mut self.terminal {
            terminal.draw(|f| {
                f.render_widget(Paragraph::new("").alignment(Alignment::Left), f.area());
            })?;
        }
        Ok(())
    }

    /// Process a single event
    pub async fn handle_event(&mut self, event: Event) -> crate::app::Result<()> {
        match event {
            Event::Key(key) => match self.app.input_mode() {
                InputMode::Normal => self.handle_normal_mode_key(key),
                InputMode::Insert => self.handle_insert_mode_key(key),
            },
            Event::Tick => {}
            Event::System(_, _) => {}
            _ => {}
        }

        if self.app.should_quit() {
            self.should_quit = true;
        }
        Ok(())
    }

    fn handle_normal_mode_key(&mut self, key: crossterm::event::KeyEvent) {
        use crossterm::event::KeyCode;
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => {
                self.app.quit();
            }
            KeyCode::Char('i') => {
                self.app.set_input_mode(InputMode::Insert);
            }
            KeyCode::Tab => {
                match self.app.mode() {
                    crate::app::AppMode::Normal => {
                        self.app.set_mode(crate::app::AppMode::AgentPanel)
                    }
                    crate::app::AppMode::AgentPanel => {
                        self.app.set_mode(crate::app::AppMode::Normal)
                    }
                    _ => self.app.set_mode(crate::app::AppMode::Normal),
                }
            }
            KeyCode::Char('a') => {
                self.app.set_mode(crate::app::AppMode::AgentPanel);
            }
            _ => {}
        }
    }

    fn handle_insert_mode_key(&mut self, key: crossterm::event::KeyEvent) {
        use crossterm::event::KeyCode;
        match key.code {
            KeyCode::Esc => {
                self.app.set_input_mode(InputMode::Normal);
            }
            KeyCode::Enter => {
                self.app.submit_input();
            }
            KeyCode::Char(c) => {
                self.app.add_char(c);
            }
            KeyCode::Backspace => {
                self.app.remove_char();
            }
            _ => {}
        }
    }

    /// Run the main event loop
    pub async fn run(&mut self) -> crate::app::Result<()> {
        while !self.should_quit {
            let event = crate::event::EventHandler::new(self._tick_rate)
                .next_event()
                .await;
            self.handle_event(event).await?;
        }
        Ok(())
    }
}

impl Default for TuiLoop<TestBackend> {
    fn default() -> Self {
        Self::new()
    }
}
