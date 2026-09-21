//! Input buffer, cursor, and command-history state on `KodApp`.
//!
//! Extracted from `app/mod.rs` (S9). Methods here deal with the
//! single-line/multiline text editor at the bottom of the TUI:
//! character insertion, cursor motion, word/line deletion, and the
//! up/down history ring. They do not touch messages or generation
//! state.

use super::*;

impl KodApp {
    pub fn input_mode(&self) -> &InputMode {
        &self.input_mode
    }

    pub fn set_input_mode(&mut self, mode: InputMode) {
        self.input_mode = mode;
    }

    pub fn input(&self) -> &str {
        &self.input
    }

    pub fn set_input(&mut self, input: String) {
        self.input = input;
        self.cursor_position = self.input.len();
    }

    pub fn add_char(&mut self, c: char) {
        self.input.insert(self.cursor_position, c);
        self.cursor_position += c.len_utf8();
    }

    /// Insert a newline at the cursor (Ctrl+J / Alt+Enter in insert mode).
    /// Enter still submits — multiline never traps the user.
    pub fn insert_newline(&mut self) {
        self.add_char('\n');
    }

    pub fn cursor_position(&self) -> usize {
        self.cursor_position
    }

    pub fn move_cursor_left(&mut self) {
        if self.cursor_position > 0 {
            // Step back one full char, never into the middle of UTF-8.
            let mut pos = self.cursor_position - 1;
            while pos > 0 && !self.input.is_char_boundary(pos) {
                pos -= 1;
            }
            self.cursor_position = pos;
        }
    }

    pub fn move_cursor_right(&mut self) {
        if self.cursor_position < self.input.len() {
            let mut pos = self.cursor_position + 1;
            while pos < self.input.len() && !self.input.is_char_boundary(pos) {
                pos += 1;
            }
            self.cursor_position = pos;
        }
    }

    /// Delete the word before the cursor (Ctrl+W).
    pub fn delete_word_before(&mut self) {
        if self.cursor_position == 0 {
            return;
        }
        let mut start = self.cursor_position;
        let bytes = self.input.as_bytes();
        while start > 0 && bytes[start - 1] == b' ' {
            start -= 1;
        }
        while start > 0 && bytes[start - 1] != b' ' && bytes[start - 1] != b'\n' {
            start -= 1;
        }
        self.input.drain(start..self.cursor_position);
        self.cursor_position = start;
    }

    /// Delete everything from the cursor to the start of its line (Ctrl+U).
    pub fn delete_to_line_start(&mut self) {
        let line_start = self.input[..self.cursor_position]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        self.input.drain(line_start..self.cursor_position);
        self.cursor_position = line_start;
    }

    /// (line, col) of the cursor for multiline rendering.
    pub fn cursor_line_col(&self) -> (usize, usize) {
        let before = &self.input[..self.cursor_position.min(self.input.len())];
        let line = before.chars().filter(|c| *c == '\n').count();
        let col = before.rsplit('\n').next().unwrap_or("").chars().count();
        (line, col)
    }

    /// Rows the input box needs (header + content, clamped for layout).
    pub fn input_height_rows(&self) -> u16 {
        let lines = self.input.lines().count().max(1);
        // +2 for the box border.
        (lines as u16 + 2).clamp(3, 7)
    }

    pub fn is_multiline_input(&self) -> bool {
        self.input.contains('\n')
    }

    pub fn backspace(&mut self) {
        if self.cursor_position > 0 {
            self.move_cursor_left();
            self.input.remove(self.cursor_position);
        }
    }

    /// Delete the character under the cursor (Delete key), leaving
    /// `cursor_position` where it was.
    ///
    /// The TUI used to route the Delete key through `set_input`, which
    /// resets `cursor_position` to `input.len()`. That was observable:
    /// placing the cursor mid-word and pressing Delete jumped the caret
    /// to the end of the line. This method edits in place like
    /// `backspace` does, and walks forward to the next char boundary so
    /// a non-ASCII character is removed whole.
    pub fn delete_at_cursor(&mut self) {
        let pos = self.cursor_position;
        if pos >= self.input.len() {
            return;
        }
        let mut end = pos + 1;
        while end < self.input.len() && !self.input.is_char_boundary(end) {
            end += 1;
        }
        self.input.drain(pos..end);
    }

    /// Remove the message currently being edited from history tracking so
    /// Up/Down starts over (used after `/edit` loads an old message).
    pub fn reset_history_index(&mut self) {
        self.history_index = None;
    }

    pub fn clear_input(&mut self) {
        self.input.clear();
        self.cursor_position = 0;
    }

    pub fn submit_input(&mut self) {
        if !self.input.is_empty() {
            self.input_history.push(self.input.clone());

            self.add_message(Message {
                id: MessageId::new(),
                role: MessageRole::User,
                content: self.input.clone(),
                timestamp: Utc::now(),
                metadata: MessageMetadata::default(),
                sequence: 0,
            });

            self.clear_input();
            self.history_index = None;
        }
    }

    pub fn input_history(&self) -> &[String] {
        &self.input_history
    }

    pub fn history_previous(&mut self) {
        if self.input_history.is_empty() {
            return;
        }

        match self.history_index {
            None => {
                self.draft = self.input.clone();
                self.history_index = Some(self.input_history.len() - 1);
                self.set_input(self.input_history[self.input_history.len() - 1].clone());
            }
            Some(0) => {}
            Some(index) => {
                self.history_index = Some(index - 1);
                self.set_input(self.input_history[index - 1].clone());
            }
        }
    }

    pub fn history_next(&mut self) {
        if self.input_history.is_empty() {
            return;
        }

        match self.history_index {
            None => {}
            Some(index) if index >= self.input_history.len() - 1 => {
                self.history_index = None;
                // Restore the stashed draft, not a blank box.
                let draft = std::mem::take(&mut self.draft);
                self.set_input(draft);
            }
            Some(index) => {
                self.history_index = Some(index + 1);
                self.set_input(self.input_history[index + 1].clone());
            }
        }
    }

    /// Move the cursor one word left (Ctrl+Left).
    pub fn move_cursor_word_left(&mut self) {
        if self.cursor_position == 0 {
            return;
        }
        let bytes = self.input.as_bytes();
        let mut pos = self.cursor_position;
        while pos > 0 && bytes[pos - 1] == b' ' {
            pos -= 1;
        }
        while pos > 0 && bytes[pos - 1] != b' ' && bytes[pos - 1] != b'\n' {
            pos -= 1;
        }
        while pos > 0 && !self.input.is_char_boundary(pos) {
            pos -= 1;
        }
        self.cursor_position = pos;
    }

    /// Move the cursor one word right (Ctrl+Right).
    pub fn move_cursor_word_right(&mut self) {
        let len = self.input.len();
        if self.cursor_position >= len {
            return;
        }
        let bytes = self.input.as_bytes();
        let mut pos = self.cursor_position;
        while pos < len && bytes[pos] != b' ' && bytes[pos] != b'\n' {
            pos += 1;
        }
        while pos < len && bytes[pos] == b' ' {
            pos += 1;
        }
        while pos < len && !self.input.is_char_boundary(pos) {
            pos += 1;
        }
        self.cursor_position = pos;
    }

    /// Delete from the cursor to the end of its line (Ctrl+K).
    pub fn cut_to_end(&mut self) {
        let end = self.input[self.cursor_position..]
            .find('\n')
            .map(|i| self.cursor_position + i)
            .unwrap_or(self.input.len());
        self.input.drain(self.cursor_position..end);
    }

    /// Delete the whole input line(s) (Ctrl+U clears to start; this clears
    /// everything — used when the box holds a failed one-liner).
    pub fn clear_line(&mut self) {
        self.clear_input();
    }
}
