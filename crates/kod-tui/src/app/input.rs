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
        // Wholesale replacement: any paste ranges referred to the old
        // text (history recall, retry, programmatic set).
        self.pasted_blocks.clear();
    }

    pub fn add_char(&mut self, c: char) {
        let pos = self.cursor_position.min(self.input.len());
        self.cursor_position = pos;
        self.shift_paste_blocks(pos, pos, c.len_utf8());
        self.input.insert(pos, c);
        self.cursor_position = pos + c.len_utf8();
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
        self.shift_paste_blocks(start, self.cursor_position, 0);
        self.input.drain(start..self.cursor_position);
        self.cursor_position = start;
    }

    /// Delete everything from the cursor to the start of its line (Ctrl+U).
    pub fn delete_to_line_start(&mut self) {
        let line_start = self.input[..self.cursor_position]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        self.shift_paste_blocks(line_start, self.cursor_position, 0);
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

    /// Rows the input box needs: wrapped visual rows of the shared
    /// view builder (paste chips collapse, long lines wrap) plus the
    /// box border, clamped so the box grows with content but never
    /// eats the chat.
    pub fn input_height_rows(&self, total_width: u16) -> u16 {
        let inner = total_width.saturating_sub(2) as usize;
        let rows = self.input_view(inner).rows.len().max(1);
        // Saturate on the cast, not on `as u16`. A bracketed paste of
        // 65536+ newlines makes `rows` a value that `rows as u16`
        // truncates into [0, 3) — `+ 2` then clamps to 3, i.e. the
        // box collapses from `INPUT_MAX_ROWS` (10) to the minimum
        // height on the largest inputs, the exact opposite of what
        // the clamp is for. Same shape as the popup-height fix in
        // `ui/question.rs` and its siblings.
        u16::try_from(rows)
            .unwrap_or(u16::MAX)
            .saturating_add(2)
            .clamp(3, INPUT_MAX_ROWS)
    }

    pub fn is_multiline_input(&self) -> bool {
        self.input.contains('\n')
    }

    pub fn backspace(&mut self) {
        if self.cursor_position > 0 {
            let old = self.cursor_position.min(self.input.len());
            self.cursor_position = old;
            self.move_cursor_left();
            let new = self.cursor_position;
            self.shift_paste_blocks(new, old, 0);
            self.input.drain(new..old);
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
        let pos = self.cursor_position.min(self.input.len());
        self.cursor_position = pos;
        if pos >= self.input.len() {
            return;
        }
        let mut end = pos + 1;
        while end < self.input.len() && !self.input.is_char_boundary(end) {
            end += 1;
        }
        self.shift_paste_blocks(pos, end, 0);
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
        self.pasted_blocks.clear();
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
        let cursor = self.cursor_position.min(self.input.len());
        self.cursor_position = cursor;
        let end = self.input[cursor..]
            .find('\n')
            .map(|i| cursor + i)
            .unwrap_or(self.input.len());
        self.shift_paste_blocks(cursor, end, 0);
        self.input.drain(cursor..end);
    }

    /// Delete the whole input line(s) (Ctrl+U clears to start; this clears
    /// everything — used when the box holds a failed one-liner).
    pub fn clear_line(&mut self) {
        self.clear_input();
    }

    /// Insert a bracketed-paste payload at the cursor as one unit:
    /// CR bytes are dropped (CRLF pastes arrive with both), existing
    /// paste ranges shift right, and the new range is recorded (fused
    /// with an adjacent previous paste from a split delivery).
    pub fn insert_paste(&mut self, text: &str) {
        let clean: String = text.chars().filter(|&c| c != '\r').collect();
        if clean.is_empty() {
            return;
        }
        let pos = self.cursor_position.min(self.input.len());
        self.cursor_position = pos;
        self.shift_paste_blocks(pos, pos, clean.len());
        self.input.insert_str(pos, &clean);
        let end = pos + clean.len();
        self.cursor_position = end;
        // Fuse with an adjacent previous block: terminals may deliver
        // one paste as several events; it is still one paste.
        let mut start = pos;
        if let Some(prev) = self.pasted_blocks.last()
            && prev.end == pos
        {
            start = prev.start;
            self.pasted_blocks.pop();
        }
        self.pasted_blocks.push(PastedBlock { start, end });
    }

    /// Adjust paste ranges for an edit replacing `[edit_start,
    /// edit_end)` with `new_len` bytes (byte offsets in the pre-edit
    /// text). Ranges after the edit shift; ranges overlapped by it are
    /// dropped — the chip dissolves and the text shows verbatim.
    fn shift_paste_blocks(&mut self, edit_start: usize, edit_end: usize, new_len: usize) {
        let delta = new_len as isize - (edit_end - edit_start) as isize;
        self.pasted_blocks.retain_mut(|b| {
            if b.end <= edit_start {
                true
            } else if b.start >= edit_end {
                b.start = (b.start as isize + delta).max(0) as usize;
                b.end = (b.end as isize + delta).max(0) as usize;
                true
            } else {
                false
            }
        });
    }
}

/// A bracketed-paste insertion still present in the input buffer (see
/// `insert_paste` / `shift_paste_blocks`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PastedBlock {
    pub start: usize,
    pub end: usize,
}

/// Pasted blocks spanning more than this many lines render as a
/// single `[Pasted N lines]` chip. Short pastes stay verbatim — a
/// two-line snippet is worth seeing.
const PASTE_COLLAPSE_LINES: usize = 3;

/// Max box height in rows including the border. The box grows with
/// wrapped content up to this, then scrolls inside it.
const INPUT_MAX_ROWS: u16 = 10;

/// One rendered visual row of the input box.
#[derive(Debug, Clone)]
pub struct InputViewRow {
    /// Cell-ready text (tabs expanded, no newlines). A chip row holds
    /// e.g. `fix [Pasted 12 lines] please` — typed text around the
    /// chip stays visible.
    pub text: String,
    /// Whether this is the first visual row of its logical line
    /// (gets the prompt; wrapped continuations are bare).
    pub first: bool,
    /// Logical line index (0 → `❯ `, rest → `… `).
    pub logical: usize,
    /// Cursor offset in CELLS within `text`, when the cursor is here.
    pub cursor_cell: Option<usize>,
    /// Chip spans as char ranges within `text` (first chunk only),
    /// rendered dim. Empty when the cursor shares the row — the
    /// cursor wins over styling in that transient state.
    pub chips: Vec<(usize, usize)>,
}

/// The input box as the widget draws it: wrapped, chip-collapsed
/// rows plus the cursor mapped into them. Built once per frame and
/// shared by the height computation and the widget so the two can
/// never disagree about what fits.
#[derive(Debug, Clone)]
pub struct InputView {
    pub rows: Vec<InputViewRow>,
    /// Index into `rows` holding the cursor.
    pub cursor_row: usize,
}

/// Terminal cell width of one char. ASCII fast path; the wide ranges
/// cover CJK + emoji (a best effort for the input box — chat
/// rendering leans on ratatui's own wrapping).
fn cell_width(c: char) -> usize {
    let u = c as u32;
    if u < 0x20 || (0x7F..0xA0).contains(&u) {
        0
    } else if matches!(u,
        0x1100..=0x115F
        | 0x2E80..=0x303E
        | 0x3041..=0x33FF
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xA000..=0xA4CF
        | 0xAC00..=0xD7AF
        | 0xF900..=0xFAFF
        | 0xFE30..=0xFE4F
        | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x1F300..=0x1F64F
        | 0x1F680..=0x1FAFF
        | 0x20000..=0x2FFFD
        | 0x30000..=0x3FFFD
    ) {
        2
    } else {
        1
    }
}

fn cells(s: &str) -> usize {
    s.chars().map(cell_width).sum()
}

/// Display expansion of a buffer slice: tabs become spaces so cursor
/// math and wrapping agree with what the terminal shows.
fn expand_slice(s: &str) -> String {
    s.replace('\t', "    ")
}

impl KodApp {
    /// Build the drawable view of the input box for `inner_width`
    /// content cells: paste chips collapse, long lines wrap, and the
    /// cursor maps to (row, cell). Pure — no state touched, safe to
    /// call from both layout and render.
    pub fn input_view(&self, inner_width: usize) -> InputView {
        let wrap_w = inner_width.max(8);
        let input = &self.input;
        let cb = self.cursor_position.min(input.len());

        // Logical line byte ranges ([start, end), end excludes '\n').
        let mut starts: Vec<usize> = vec![0];
        for (i, b) in input.bytes().enumerate() {
            if b == b'\n' {
                starts.push(i + 1);
            }
        }
        let ends: Vec<usize> = starts
            .iter()
            .enumerate()
            .map(|(i, _)| {
                if i + 1 < starts.len() {
                    starts[i + 1] - 1
                } else {
                    input.len()
                }
            })
            .collect();
        let line_idx_of = |byte: usize| -> usize {
            let mut lo = 0;
            for (i, &s) in starts.iter().enumerate() {
                if s <= byte {
                    lo = i;
                } else {
                    break;
                }
            }
            lo
        };

        // Valid, sorted paste ranges (edits maintain these; clamp
        // defensively so a stale range can never panic us).
        let mut blocks: Vec<PastedBlock> = self
            .pasted_blocks
            .iter()
            .copied()
            .filter(|b| b.start < b.end && b.start <= input.len() && b.end <= input.len())
            .collect();
        blocks.sort_by_key(|b| b.start);

        struct LogRow {
            text: String,
            cursor_char: Option<usize>,
            chips: Vec<(usize, usize)>,
            logical: usize,
        }
        let mut log_rows: Vec<LogRow> = Vec::new();
        let mut i = 0;
        while i < starts.len() {
            let mut row = String::new();
            let mut cursor_char: Option<usize> = None;
            let mut chips: Vec<(usize, usize)> = Vec::new();
            let mut pos = starts[i];
            let mut lend = ends[i];
            // Byte offset → expanded char offset of the cursor within
            // `input[from..to]`, for mapping the cursor into the row.
            let cursor_in = |row_chars: usize, from: usize, to: usize| -> usize {
                row_chars + expand_slice(&input[from..cb.min(to)]).chars().count()
            };
            loop {
                let nb = blocks
                    .iter()
                    .find(|b| b.start <= lend && b.end > pos)
                    .copied();
                match nb {
                    None => {
                        let base = row.chars().count();
                        row.push_str(&expand_slice(&input[pos..lend]));
                        if cursor_char.is_none() && cb >= pos && cb <= lend {
                            cursor_char = Some(cursor_in(base, pos, lend));
                        }
                        break;
                    }
                    Some(b) => {
                        if b.start > pos {
                            let e = b.start.min(lend);
                            let base = row.chars().count();
                            row.push_str(&expand_slice(&input[pos..e]));
                            if cursor_char.is_none() && cb >= pos && cb <= e {
                                cursor_char = Some(cursor_in(base, pos, e));
                            }
                            pos = e;
                        }
                        let first = line_idx_of(b.start);
                        let last = line_idx_of(b.end - 1);
                        let n = last - first + 1;
                        if n > PASTE_COLLAPSE_LINES {
                            let chip_start = row.chars().count();
                            let chip = format!("[Pasted {n} lines]");
                            row.push_str(&chip);
                            chips.push((chip_start, chip_start + chip.chars().count()));
                            if cursor_char.is_none() && cb >= b.start && cb < b.end {
                                // Cursor strictly inside the chip: park
                                // it just past the chip text. A cursor
                                // exactly at the block end belongs to
                                // the tail text handled below.
                                cursor_char = Some(row.chars().count());
                            }
                            // Consume through the block's last line, keep
                            // the row open for its tail text. When the
                            // block swallowed the line's newline too,
                            // nothing remains on this line.
                            i = last;
                            pos = b.end;
                            lend = ends[i];
                            if pos > lend {
                                break;
                            }
                            continue;
                        } else {
                            let e = b.end.min(lend);
                            let base = row.chars().count();
                            row.push_str(&expand_slice(&input[pos..e]));
                            if cursor_char.is_none() && cb >= pos && cb <= e {
                                cursor_char = Some(cursor_in(base, pos, e));
                            }
                            pos = e;
                            if b.end > lend {
                                // Block continues on later lines.
                                break;
                            }
                        }
                    }
                }
            }
            log_rows.push(LogRow {
                text: row,
                cursor_char,
                chips,
                logical: i,
            });
            i += 1;
        }

        // Wrap logical rows into width chunks, mapping the cursor. The
        // first chunk of each logical row reserves two cells for the
        // `❯ `/`… ` prompt; continuations are bare.
        let mut rows: Vec<InputViewRow> = Vec::new();
        let mut cursor_row = 0;
        let first_w = wrap_w.saturating_sub(2).max(1);
        for log in &log_rows {
            let chars: Vec<char> = log.text.chars().collect();
            let mut k = 0; // char index into this logical row
            let mut chunk = String::new();
            let mut chunk_cells = 0;
            let mut chunk_cursor: Option<usize> = None;
            let mut first = true;
            // Push the open chunk as a visual row. `had_cursor` must
            // be read before the move into the row; the caller resets
            // the accumulators when the loop continues.
            macro_rules! flush {
                () => {{
                    let had_cursor = chunk_cursor.is_some();
                    rows.push(InputViewRow {
                        text: std::mem::take(&mut chunk),
                        first,
                        logical: log.logical,
                        cursor_cell: chunk_cursor.take(),
                        // Chip ranges only survive on the first chunk
                        // (chips are short; a wrapped chip restyles as
                        // plain text on continuations).
                        chips: if first { log.chips.clone() } else { Vec::new() },
                    });
                    if had_cursor {
                        cursor_row = rows.len() - 1;
                    }
                }};
            }
            // An empty logical row still yields one (empty) visual row
            // so blank lines keep their height.
            loop {
                if k >= chars.len() {
                    if log.cursor_char == Some(chars.len()) && chunk_cursor.is_none() {
                        chunk_cursor = Some(chunk_cells);
                    }
                    flush!();
                    break;
                }
                if log.cursor_char == Some(k) && chunk_cursor.is_none() {
                    chunk_cursor = Some(chunk_cells);
                }
                let c = chars[k];
                let w = cell_width(c);
                let lim = if first { first_w } else { wrap_w };
                if chunk_cells + w > lim && !chunk.is_empty() {
                    flush!();
                    chunk_cells = 0;
                    first = false;
                    continue;
                }
                // A single char wider than the limit (tiny terminal)
                // overflows rather than vanishing.
                chunk.push(c);
                chunk_cells += w;
                k += 1;
            }
        }
        // The cursor must always land somewhere: if no row claimed
        // it (unreachable in practice — every byte maps to exactly
        // one row), pin it to the end of the last row.
        if !rows.iter().any(|r| r.cursor_cell.is_some()) {
            if rows.is_empty() {
                rows.push(InputViewRow {
                    text: String::new(),
                    first: true,
                    logical: 0,
                    cursor_cell: Some(0),
                    chips: Vec::new(),
                });
            }
            let last = rows.len() - 1;
            cursor_row = last;
            rows[last].cursor_cell = Some(cells(&rows[last].text));
        }
        InputView { rows, cursor_row }
    }
}
