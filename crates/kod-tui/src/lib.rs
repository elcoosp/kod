//! Terminal UI for KOD - provides interactive interface.
//!
//! This crate implements the terminal user interface using Ratatui,
//! including chat display, agent panel, input handling, and tool execution display.

pub mod app;
pub mod components;
pub mod event;
pub mod main_loop;
pub mod ui;

pub use app::{AppMode, InputMode, KodApp, Message};
pub use event::{Event, EventHandler, EventPriority, KeyCode};
pub use main_loop::TuiLoop;
