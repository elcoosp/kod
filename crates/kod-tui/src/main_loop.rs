//! Main TUI loop - coordinates rendering and event handling.

use crate::{
    app::{AppMode, InputMode, KodApp},
    event::{Event, EventHandler, KeyCode},
    ui::{AgentPanelWidget, ChatWidget, InputWidget},
};
use kod_error::{KodError, Result};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    Terminal,
};
use std::io::Stdout;
use std::time::Duration;

/// Main TUI application loop
pub struct TuiLoop {
    app: KodApp,
    event_handler: EventHandler,
    terminal: Option<Terminal<CrosstermBackend<Stdout>>>,
}

impl TuiLoop {
    pub fn new() -> Self {
        Self {
            app: KodApp::new(),
            event_handler: EventHandler::new(Duration::from_millis(100)),
            terminal: None,
        }
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

    /// Initialize terminal
    pub async fn init_terminal(&mut self) -> Result<()> {
        crossterm::terminal::enable_raw_mode()
            .map_err(|e| KodError::Internal(format!("Failed to enable raw mode: {}", e)))?;

        crossterm::execute!(
            std::io::stdout(),
            crossterm::terminal::EnterAlternateScreen,
            crossterm::event::EnableMouseCapture
        ).map_err(|e| KodError::Internal(format!("Failed to enter alternate screen: {}", e)))?;

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
            terminal.show_cursor()
                .map_err(|e| KodError::Internal(format!("Failed to show cursor: {}", e)))?;
        }

        crossterm::execute!(
            std::io::stdout(),
            crossterm::terminal::LeaveAlternateScreen,
            crossterm::event::DisableMouseCapture
        ).map_err(|e| KodError::Internal(format!("Failed to leave alternate screen: {}", e)))?;

        crossterm::terminal::disable_raw_mode()
            .map_err(|e| KodError::Internal(format!("Failed to disable raw mode: {}", e)))?;

        self.terminal = None;

        Ok(())
    }

    /// Run the main loop
    pub async fn run(&mut self) -> Result<()> {
        self.init_terminal().await?;

        let result = self.main_loop().await;

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

    /// Render the UI
    async fn render(&mut self) -> Result<()> {
        if let Some(terminal) = &mut self.terminal {
            terminal.draw(|f| {
                let size = f.area();

                let chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([
                        Constraint::Length(1),
                        Constraint::Min(1),
                        Constraint::Length(3),
                    ])
                    .split(size);

                let status_text = format!(
                    " KOD | Mode: {:?} | Input: {:?} | Messages: {} ",
                    self.app.mode(),
                    self.app.input_mode(),
                    self.app.messages().len()
                );

                let status = ratatui::widgets::Paragraph::new(status_text)
                    .style(ratatui::style::Style::default().fg(ratatui::style::Color::White));

                f.render_widget(status, chunks[0]);

                if *self.app.mode() == AppMode::AgentPanel {
                    let _main_chunks = Layout::default()
                        .direction(Direction::Horizontal)
                        .constraints([
                            Constraint::Percentage(70),
                            Constraint::Percentage(30),
                        ])
                        .split(chunks[1]);

                    let chat_widget = ChatWidget::new();
                    let chat_buffer = f.buffer_mut();
                    chat_widget.render(&self.app, &mut *chat_buffer);

                    let agent_widget = AgentPanelWidget::new();
                    let agent_buffer = f.buffer_mut();
                    agent_widget.render(&self.app, &mut *agent_buffer);
                } else {
                    let chat_widget = ChatWidget::new();
                    let chat_buffer = f.buffer_mut();
                    chat_widget.render(&self.app, &mut *chat_buffer);
                }

                let input_widget = InputWidget::new();
                let input_buffer = f.buffer_mut();
                input_widget.render(&self.app, &mut *input_buffer);
            }).map_err(|e| KodError::Internal(format!("Failed to draw: {}", e)))?;
        }

        Ok(())
    }

    /// Handle a single event
    pub async fn handle_event(&mut self, event: Event) -> Result<()> {
        match event {
            Event::Key(key_code) => self.handle_key(key_code).await?,
            Event::UserInput(input) => {
                self.app.set_input(input);
                self.app.submit_input();
            }
            Event::Quit => {
                self.app.quit();
            }
            Event::Tick => {}
            Event::Resize(w, h) => {
                tracing::debug!("Terminal resized to {}x{}", w, h);
            }
            Event::ResponseChunk(chunk) => {
                self.app.add_response_chunk(&chunk);
            }
            Event::ResponseComplete(_) => {
                self.app.complete_response();
            }
            Event::ToolStarted(tool_name) => {
                self.app.start_tool_execution(&tool_name);
            }
            Event::ToolCompleted(tool_name, result) => {
                self.app.complete_tool_execution(&tool_name, &result);
            }
            Event::AgentMessage(agent_name, message) => {
                self.app.add_message(crate::app::Message {
                    id: kod_types::MessageId::new(),
                    role: kod_types::MessageRole::Agent(kod_types::AgentId::new()),
                    content: format!("[{}] {}", agent_name, message),
                    timestamp: chrono::Utc::now(),
                    metadata: Default::default(),
                });
            }
            Event::Error(error) => {
                self.app.add_message(crate::app::Message {
                    id: kod_types::MessageId::new(),
                    role: kod_types::MessageRole::System,
                    content: format!("Error: {}", error),
                    timestamp: chrono::Utc::now(),
                    metadata: Default::default(),
                });
            }
            _ => {}
        }

        Ok(())
    }

    /// Handle key events
    async fn handle_key(&mut self, key: KeyCode) -> Result<()> {
        match self.app.input_mode() {
            InputMode::Normal => self.handle_normal_mode_key(key).await,
            InputMode::Insert => self.handle_insert_mode_key(key).await,
        }
    }

    /// Handle keys in normal mode
    async fn handle_normal_mode_key(&mut self, key: KeyCode) -> Result<()> {
        match key {
            KeyCode::Char('i') => {
                self.app.set_input_mode(InputMode::Insert);
            }
            KeyCode::Char('q') | KeyCode::Escape => {
                self.app.quit();
            }
            KeyCode::Tab => {
                match self.app.mode() {
                    AppMode::Normal => self.app.set_mode(AppMode::AgentPanel),
                    AppMode::AgentPanel => self.app.set_mode(AppMode::Normal),
                    _ => self.app.set_mode(AppMode::Normal),
                }
            }
            KeyCode::Char('h') | KeyCode::Char('?') => {
                self.app.set_mode(AppMode::Help);
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
                self.app.scroll_to_bottom();
            }
            _ => {}
        }

        Ok(())
    }

    /// Handle keys in insert mode
    async fn handle_insert_mode_key(&mut self, key: KeyCode) -> Result<()> {
        match key {
            KeyCode::Enter => {
                self.app.submit_input();
            }
            KeyCode::Escape => {
                self.app.set_input_mode(InputMode::Normal);
            }
            KeyCode::Backspace => {
                self.app.backspace();
            }
            KeyCode::Up => {
                self.app.history_previous();
            }
            KeyCode::Down => {
                self.app.history_next();
            }
            KeyCode::Char(c) => {
                self.app.add_char(c);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_tui_lifecycle() {
        let mut tui = TuiLoop::new();

        tui.handle_event(Event::Key(KeyCode::Char('i'))).await.unwrap();
        assert_eq!(tui.app().input_mode(), &InputMode::Insert);

        tui.handle_event(Event::Key(KeyCode::Escape)).await.unwrap();
        assert_eq!(tui.app().input_mode(), &InputMode::Normal);
    }
}
