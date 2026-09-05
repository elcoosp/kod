//! UI rendering components.

pub mod agent_panel;
pub mod chat;
pub mod completions;
pub mod header;
pub mod help;
pub mod input;
pub mod status;

pub use agent_panel::AgentPanelWidget;
pub use chat::ChatWidget;
pub use completions::CompletionsWidget;
pub use header::HeaderWidget;
pub use help::HelpWidget;
pub use input::InputWidget;
pub use status::StatusWidget;
