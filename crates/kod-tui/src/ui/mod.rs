//! UI widgets for the TUI.

pub mod agent_panel;
pub mod chat;
pub mod header;
pub mod input;

use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::Frame;

use crate::app::KodApp;

/// Draw the full UI frame
pub fn draw(app: &KodApp, frame: &mut Frame) {
    let _chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([Constraint::Min(3), Constraint::Min(1), Constraint::Max(3)].as_ref())
        .split(frame.area());

    // Header
    header::HeaderWidget::new().render(app, frame.buffer_mut());

    // Chat area
    chat::ChatWidget::new().render(app, frame.buffer_mut());

    // Input area
    input::InputWidget::new().render(app, frame.buffer_mut());
}
