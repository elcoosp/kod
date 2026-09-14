#!/usr/bin/env bash
set -uo pipefail

COMPILE_OK=true
APP=crates/kod-tui/src/app.rs
LOOP=crates/kod-tui/src/main_loop.rs

for f in "$APP" "$LOOP"; do
    if [ ! -f "$f" ]; then
        echo "ERROR: missing $f — run from the kod workspace root"
        exit 1
    fi
done

echo "Fixing Delete cursor position; hiding the terminal cursor"

python3 - "$APP" "$LOOP" << 'PYEOF'
import os
import sys

app, loop = sys.argv[1], sys.argv[2]

def patch(path, old, new, label, expect=1):
    with open(path, "r") as f:
        content = f.read()
    n = content.count(old)
    if n == 0:
        print(f"ERROR: old snippet not found in {path}: {label}")
        sys.exit(2)
    if expect and n != expect:
        print(f"ERROR: expected {expect} occurrence(s) of {label} in {path}, found {n}")
        sys.exit(2)
    patched = content.replace(old, new, expect if expect else n)
    tmp = path + ".tmp"
    with open(tmp, "w") as f:
        f.write(patched)
    os.replace(tmp, path)
    print(f"Patched {path}: {label}")

# ========================================================================
# 1. Add KodApp::delete_at_cursor — Delete key, preserving cursor.
# ========================================================================
patch(
    app,
    '''    pub fn backspace(&mut self) {
        if self.cursor_position > 0 {
            self.move_cursor_left();
            self.input.remove(self.cursor_position);
        }
    }''',
    '''    pub fn backspace(&mut self) {
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
    }''',
    "delete_at_cursor",
)

# ========================================================================
# 2. TuiLoop: Delete key uses the new method.
# ========================================================================
patch(
    loop,
    '''            KeyCode::Delete => {
                if self.app.cursor_position() < self.app.input().len() {
                    let pos = self.app.cursor_position();
                    self.app.set_input({
                        let mut s = self.app.input().to_string();
                        if let Some((idx, ch)) = s[pos..].char_indices().next() {
                            s.drain(pos..idx + ch.len_utf8());
                        }
                        s
                    });
                }
            }''',
    '''            KeyCode::Delete => {
                // Uses the in-place delete method so the cursor stays
                // where it was. The previous implementation called
                // `set_input`, which resets `cursor_position` to the end
                // of the input — a visible jump on every Delete press.
                self.app.delete_at_cursor();
                self.app.reset_completion();
            }''',
    "Delete key routes to delete_at_cursor",
)

# ========================================================================
# 3. Hide the terminal's own cursor.
# ========================================================================
patch(
    loop,
    '''        let backend = CrosstermBackend::new(std::io::stdout());
        let terminal = Terminal::new(backend)
            .map_err(|e| KodError::Internal(format!("Failed to create terminal: {}", e)))?;

        self.terminal = Some(terminal);''',
    '''        let backend = CrosstermBackend::new(std::io::stdout());
        let mut terminal = Terminal::new(backend)
            .map_err(|e| KodError::Internal(format!("Failed to create terminal: {}", e)))?;

        // Hide the terminal's own cursor. InputWidget draws an inline
        // `▌` at the current position, and leaving the real cursor
        // visible produced two carets on screen — one at the input
        // box and one wherever ratatui last placed the hardware cursor.
        // `restore_terminal` calls `show_cursor` on the way out.
        let _ = terminal.hide_cursor();

        self.terminal = Some(terminal);''',
    "hide terminal cursor",
)

# ========================================================================
# 4. Test: delete preserves cursor position.
# ========================================================================
patch(
    loop,
    '''    /// The default binding set must keep 'i' as insert, so a fresh''',
    '''    /// Delete key (insert mode) must remove the character under the
    /// cursor and leave the cursor where it was. Regression: the
    /// previous implementation routed through `set_input`, which snaps
    /// `cursor_position` to the end of the input.
    #[tokio::test]
    async fn test_delete_preserves_cursor_position() {
        let mut tui = TuiLoop::new();
        tui.app_mut().set_input_mode(InputMode::Insert);
        tui.app_mut().set_input("hello world".to_string());
        // Cursor is at end after set_input; move left to sit on 'w'.
        for _ in 0..5 {
            tui.handle_event(Event::Key(KeyCode::Left)).await.unwrap();
        }
        assert_eq!(tui.app().cursor_position(), 6);

        tui.handle_event(Event::Key(KeyCode::Delete)).await.unwrap();
        assert_eq!(tui.app().input(), "hello orld");
        assert_eq!(
            tui.app().cursor_position(),
            6,
            "cursor must stay put after Delete, not jump to end"
        );
    }

    /// Backspace still removes the character before the cursor and
    /// moves the cursor left by one — pin against future changes.
    #[tokio::test]
    async fn test_backspace_still_moves_cursor_left() {
        let mut tui = TuiLoop::new();
        tui.app_mut().set_input_mode(InputMode::Insert);
        tui.app_mut().set_input("hello".to_string());
        tui.handle_event(Event::Key(KeyCode::Backspace)).await.unwrap();
        assert_eq!(tui.app().input(), "hell");
        assert_eq!(tui.app().cursor_position(), 4);
    }

    /// The default binding set must keep 'i' as insert, so a fresh''',
    "delete cursor tests",
)

print("All patches applied.")
PYEOF

if [ $? -ne 0 ]; then
    echo "ERROR: patching failed"
    exit 1
fi

echo "cargo check --workspace --all-targets"
if ! cargo check --workspace --all-targets 2>&1; then
    echo "Compilation failed"
    exit 1
fi

echo "cargo clippy --workspace --all-targets -- -D warnings"
if ! cargo clippy --workspace --all-targets -- -D warnings 2>&1; then
    echo "Clippy failed"
    exit 1
fi

echo "Committing."
git add -A
git commit -m "fix(tui): Delete preserves cursor; hide the hardware cursor

Two input-layer issues.

1. Delete in insert mode jumped the cursor to the end of the input.
   The TUI built a new string and handed it to KodApp::set_input,
   which resets cursor_position to input.len() as a side effect (a
   sensible default when replacing the whole line, wrong when
   deleting under the caret). Add KodApp::delete_at_cursor, which
   edits in place like backspace does — including walking forward to
   the next char boundary so a non-ASCII character is removed whole
   — and route the Delete key through it. Also reset completion on
   Delete, matching backspace.

2. The terminal's own cursor was never hidden. InputWidget draws an
   inline `▌` at the current position, so the user saw two carets:
   one at the input box and one wherever ratatui last placed the
   hardware cursor. init_terminal now calls terminal.hide_cursor()
   right after construction; restore_terminal's existing
   show_cursor() puts it back on the way out.

Adds two tests: Delete leaves the cursor where it was on a mid-word
press, and Backspace still removes the character before the cursor
and moves left by one."
