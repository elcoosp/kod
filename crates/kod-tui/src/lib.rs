//! Terminal UI for KOD - provides interactive interface.
//!
//! This crate implements the terminal user interface using Ratatui,
//! including chat display, agent panel, input handling, and tool execution display.

pub mod event;
pub mod app;
pub mod ui;
pub mod components;
pub mod main_loop;

pub use event::{Event, EventHandler, EventPriority, KeyCode};
pub use app::{KodApp, Message, AppMode, InputMode};
pub use main_loop::TuiLoop;
