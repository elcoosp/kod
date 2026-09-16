//! CLI interface for KOD.
//!
//! Provides command-line entry point and command definitions.

pub mod commands;
pub mod doctor;

pub use commands::{Cli, Command};
