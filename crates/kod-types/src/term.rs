//! Session-safety flag for terminal ownership.
//!
//! The global `tracing` subscriber writes to stderr. While the TUI owns
//! the terminal (alternate screen + raw mode), any raw stderr write
//! lands at the live cursor, interleaves with the ratatui frame diff,
//! and pushes transcript rows over the input box — and because the
//! redraw is diff-based, cells it believes are unchanged are never
//! repaired, so the garble persists.
//!
//! This flag lets a shared writer (see `kod_cli::logging`) route log
//! output to a file for the lifetime of the TUI session instead of to
//! the terminal.

use std::sync::atomic::{AtomicBool, Ordering};

static TUI_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Mark the TUI as owning the terminal. Call right after entering the
/// alternate screen; clear (with `false`) before leaving it.
pub fn set_tui_active(active: bool) {
    TUI_ACTIVE.store(active, Ordering::SeqCst);
}

/// True while the TUI owns the terminal and direct stdout/stderr
/// writes are forbidden (they would garble the frame).
pub fn tui_active() -> bool {
    TUI_ACTIVE.load(Ordering::SeqCst)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flag_round_trips() {
        set_tui_active(true);
        assert!(tui_active());
        set_tui_active(false);
        assert!(!tui_active());
    }
}
