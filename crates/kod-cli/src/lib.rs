//! CLI interface for KOD.
//!
//! Provides command-line entry point and command definitions.

pub mod commands;

// `doctor` lives in `kod-core` because both `kod-cli` and `kod-tui`
// need it, and a direct dependency between the two interface crates
// would be a cycle (`kod-cli` -> `kod-tui` already exists for the
// `kod tui` subcommand). The re-export keeps `kod_cli::doctor` as a
// valid path so callers do not have to know where it moved.
pub use kod_core::doctor;

pub use commands::{Cli, Command};
