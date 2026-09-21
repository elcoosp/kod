//! Terminal UI for KOD - provides interactive interface.
//!
//! This crate implements the terminal user interface using Ratatui,
//! including chat display, agent panel, input handling, and tool execution display.

pub mod app;
pub mod clipboard;
pub mod components;
pub mod event;
pub mod keybindings;
pub mod main_loop;
pub mod markdown;
pub mod theme;
pub mod ui;

pub use app::{
    AppMode, CompletionKind, ConfirmKind, GenPhase, InputMode, KodApp, Message, SLASH_COMMANDS,
    SearchStatus, SlashCommand,
};
pub use event::{Event, EventHandler, EventPriority, KeyCode};
pub use main_loop::TuiLoop;
pub mod render_cache;
