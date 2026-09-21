//! Command palette state on `KodApp`.
//!
//! Extracted from `app/mod.rs` (S9). The palette is the Ctrl+P-style
//! fuzzy command finder; its state (`palette: Option<CommandPaletteState>`)
//! and per-keystroke navigation live here.

use super::*;

impl KodApp {
    pub fn palette(&self) -> Option<&CommandPaletteState> {
        self.palette.as_ref()
    }

    pub fn is_palette_open(&self) -> bool {
        self.palette.is_some()
    }

    /// Open the palette. Idempotent — a second call while already
    /// open is a no-op, so Ctrl+K twice does not reset the query.
    pub fn open_palette(&mut self) {
        if self.palette.is_none() {
            self.palette = Some(CommandPaletteState::new());
        }
    }

    pub fn close_palette(&mut self) {
        self.palette = None;
    }

    /// Push a character into the query.
    pub fn palette_push_char(&mut self, c: char) {
        if let Some(p) = self.palette.as_mut() {
            p.query.push(c);
            p.selected = 0;
        }
    }

    pub fn palette_backspace(&mut self) {
        if let Some(p) = self.palette.as_mut() {
            p.query.pop();
            p.selected = 0;
        }
    }

    /// Move the selection down, wrapping.
    pub fn palette_next(&mut self) {
        let len = self.palette_candidates().len();
        if len == 0 {
            return;
        }
        if let Some(p) = self.palette.as_mut() {
            p.selected = (p.selected + 1) % len;
        }
    }

    /// Move the selection up, wrapping.
    pub fn palette_prev(&mut self) {
        let len = self.palette_candidates().len();
        if len == 0 {
            return;
        }
        if let Some(p) = self.palette.as_mut() {
            p.selected = (p.selected + len - 1) % len;
        }
    }

    /// The palette's currently-filtered entries.
    pub fn palette_candidates(&self) -> Vec<CommandPaletteEntry> {
        match self.palette.as_ref() {
            Some(p) => p.filtered(&build_palette_entries()),
            None => Vec::new(),
        }
    }

    pub fn palette_query(&self) -> Option<&str> {
        self.palette.as_ref().map(|p| p.query.as_str())
    }

    pub fn palette_selected(&self) -> usize {
        self.palette.as_ref().map(|p| p.selected).unwrap_or(0)
    }

    /// The currently-selected entry, if any.
    pub fn palette_selected_entry(&self) -> Option<CommandPaletteEntry> {
        let candidates = self.palette_candidates();
        let idx = self.palette_selected();
        candidates.get(idx).cloned()
    }
}
