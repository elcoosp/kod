#!/usr/bin/env bash
set -uo pipefail

COMPILE_OK=true

echo "=== Recent commits ==="
git log --oneline -6

echo
echo "=== Feature presence checks ==="
APP=crates/kod-tui/src/app.rs
LOOP=crates/kod-tui/src/main_loop.rs

check() {
    local file="$1" needle="$2" label="$3"
    if grep -q -- "$needle" "$file"; then
        echo "  present: $label"
        return 0
    else
        echo "  MISSING: $label"
        return 1
    fi
}

MISSING=""

check "$APP" "fn delete_at_cursor" "KodApp::delete_at_cursor" || MISSING="$MISSING delete_at_cursor"
check "$LOOP" "self.app.delete_at_cursor()" "Delete key routes through delete_at_cursor" || MISSING="$MISSING delete_key"
check "$LOOP" "terminal.hide_cursor()" "init_terminal hides hardware cursor" || MISSING="$MISSING hide_cursor"
check "$LOOP" "SLASH_HELP: &str = \"" "SLASH_HELP constant exists" >/dev/null || true
check "$LOOP" "/debug last-prompt" "SLASH_HELP mentions /debug" || MISSING="$MISSING help_debug"
check "$APP" "f search" "hint_line says 'f search'" || MISSING="$MISSING hint_search"
check "$LOOP" "test_slash_help_lists_every_command" "help invariant test" || MISSING="$MISSING help_invariant"

echo
echo "Missing:${MISSING:-（none）}"

echo
echo "=== Applying any missing pieces ==="

if echo "$MISSING" | grep -q delete_at_cursor; then
    python3 - "$APP" << 'PYEOF'
import sys
path = sys.argv[1]
src = open(path).read()
old = '''    pub fn backspace(&mut self) {
        if self.cursor_position > 0 {
            self.move_cursor_left();
            self.input.remove(self.cursor_position);
        }
    }'''
new = '''    pub fn backspace(&mut self) {
        if self.cursor_position > 0 {
            self.move_cursor_left();
            self.input.remove(self.cursor_position);
        }
    }

    /// Delete the character under the cursor, leaving
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
    }'''
if old not in src:
    print("  ERROR: backspace anchor not found in app.rs")
    sys.exit(2)
open(path, "w").write(src.replace(old, new, 1))
print("  added KodApp::delete_at_cursor")
PYEOF
fi

if echo "$MISSING" | grep -q delete_key; then
    python3 - "$LOOP" << 'PYEOF'
import sys
path = sys.argv[1]
src = open(path).read()
old = '''            KeyCode::Delete => {
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
            }'''
new = '''            KeyCode::Delete => {
                // Uses the in-place delete method so the cursor stays
                // where it was. The previous implementation called
                // `set_input`, which resets `cursor_position` to the end
                // of the input — a visible jump on every Delete press.
                self.app.delete_at_cursor();
                self.app.reset_completion();
            }'''
if old not in src:
    print("  ERROR: Delete arm anchor not found in main_loop.rs")
    sys.exit(2)
open(path, "w").write(src.replace(old, new, 1))
print("  wired Delete key to delete_at_cursor")
PYEOF
fi

if echo "$MISSING" | grep -q hide_cursor; then
    python3 - "$LOOP" << 'PYEOF'
import sys
path = sys.argv[1]
src = open(path).read()
old = '''        let backend = CrosstermBackend::new(std::io::stdout());
        let terminal = Terminal::new(backend)
            .map_err(|e| KodError::Internal(format!("Failed to create terminal: {}", e)))?;

        self.terminal = Some(terminal);'''
new = '''        let backend = CrosstermBackend::new(std::io::stdout());
        let mut terminal = Terminal::new(backend)
            .map_err(|e| KodError::Internal(format!("Failed to create terminal: {}", e)))?;

        // Hide the terminal's own cursor. InputWidget draws an inline
        // block at the current position, so leaving the hardware
        // cursor visible produced two carets on screen.
        // restore_terminal calls show_cursor on the way out.
        let _ = terminal.hide_cursor();

        self.terminal = Some(terminal);'''
if old not in src:
    print("  ERROR: init_terminal anchor not found")
    sys.exit(2)
open(path, "w").write(src.replace(old, new, 1))
print("  added terminal.hide_cursor")
PYEOF
fi

if echo "$MISSING" | grep -q help_debug; then
    python3 - "$LOOP" << 'PYEOF'
import sys
path = sys.argv[1]
src = open(path).read()
# Match the whole SLASH_HELP declaration line and replace it.
old_start = 'const SLASH_HELP: &str = "'
i = src.find(old_start)
if i == -1:
    print("  ERROR: SLASH_HELP not found")
    sys.exit(2)
# The line ends with '";' (string literal at end of line, possibly
# with a following comment). Find the terminator carefully.
j = i + len(old_start)
# The value is a single string literal; find the closing `";`.
k = src.find('";', j)
if k == -1:
    print("  ERROR: SLASH_HELP closing quote not found")
    sys.exit(2)
end = k + 2  # include `";`
new_value = '''const SLASH_HELP: &str = "Commands:\\n/help — show this help\\n/clear — clear chat (asks confirm)\\n/undo — restore last /clear\\n/model [<name>] — switch model; no argument lists the server's models\\n/skills — list loaded skills\\n/goal <text> — set a goal the agent works toward until GOAL MET (/goal clear to stop)\\n/steer <instruction> — redirect the running prompt after its current tool call\\n/cancel — stop the running prompt (also Esc or Ctrl+C while it runs)\\n/compact — compact session history now\\n/retry — resend the last prompt (also r)\\n/search [<text>] — search chat (n/N next/prev, Esc clears)\\n/copy — copy last assistant reply to clipboard (also y)\\n/theme [dark|light] — cycle or set theme\\n/tools — toggle tool-output visibility (also t)\\n/debug last-prompt — write the last prompt sent to the model into ~/.kod/last_prompt.txt\\n/quit — quit kod\\n\\nWhile a prompt runs, typing + Enter steers it (same as /steer).\\nKeys: i insert · j/k or wheel scrolls · q quit · PgUp/PgDn/Home/End · g/G top/bottom · t toggle tools · o expand · y copy · r retry · u undo · f search · ? help · Esc cancel";'''
src = src[:i] + new_value + src[end:]
open(path, "w").write(src)
print("  rewrote SLASH_HELP (added /debug, fixed duplicate 'j/k scroll')")
PYEOF
fi

if echo "$MISSING" | grep -q hint_search; then
    python3 - "$APP" << 'PYEOF'
import sys
path = sys.argv[1]
src = open(path).read()
old = '"i type · / command · j/k scroll · t tools · / search · ? help · q quit".to_string()'
new = '"i type · / command · j/k scroll · t tools · f search · ? help · q quit".to_string()'
if old not in src:
    print("  ERROR: hint_line search phrase not found")
    sys.exit(2)
open(path, "w").write(src.replace(old, new, 1))
print("  fixed hint_line: 'f search'")
PYEOF
fi

if echo "$MISSING" | grep -q help_invariant; then
    python3 - "$LOOP" << 'PYEOF'
import sys
path = sys.argv[1]
src = open(path).read()
anchor = '''    /// `/theme light` must actually change the palette and report the'''
if anchor not in src:
    print("  ERROR: help-test anchor not found")
    sys.exit(2)
new_tests = '''    /// Every entry in SLASH_COMMANDS must appear in SLASH_HELP, so
    /// adding a command to the autocomplete without documenting it
    /// fails this test. The previous SLASH_HELP was missing /debug
    /// for several commits — this pins the invariant.
    #[test]
    fn test_slash_help_lists_every_command() {
        use crate::app::SLASH_COMMANDS;
        let help = SLASH_HELP;
        for cmd in SLASH_COMMANDS {
            assert!(
                help.contains(cmd.name),
                "SLASH_COMMANDS entry {:?} is not mentioned in SLASH_HELP",
                cmd.name
            );
        }
        let known: std::collections::HashSet<&'static str> =
            SLASH_COMMANDS.iter().map(|c| c.name).collect();
        for token in help.split_whitespace() {
            let trimmed = token.trim_end_matches(|c: char| {
                !c.is_ascii_alphanumeric() && c != '/' && c != '-'
            });
            if trimmed.starts_with('/') && trimmed.len() > 1 {
                assert!(
                    known.contains(trimmed),
                    "SLASH_HELP mentions {:?} which is not in SLASH_COMMANDS",
                    trimmed
                );
            }
        }
    }

    /// The idle hint line must name f as the search key, matching the
    /// default keybinding, and must not claim / starts a search.
    #[tokio::test]
    async fn test_idle_hint_names_search_key_correctly() {
        let tui = TuiLoop::new();
        let hint = tui.app().hint_line();
        assert!(
            hint.contains("f search"),
            "hint should name the f key for search: {hint}"
        );
        assert!(
            !hint.contains("/ search"),
            "hint should not claim / starts a search: {hint}"
        );
    }

    /// Delete key (insert mode) must remove the character under the
    /// cursor and leave the cursor where it was. Regression: the
    /// previous implementation routed through `set_input`, which snaps
    /// `cursor_position` to the end of the input.
    #[tokio::test]
    async fn test_delete_preserves_cursor_position() {
        let mut tui = TuiLoop::new();
        tui.app_mut().set_input_mode(InputMode::Insert);
        tui.app_mut().set_input("hello world".to_string());
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

    /// `/theme light` must actually change the palette and report the'''
src = src.replace(anchor, new_tests, 1)
open(path, "w").write(src)
print("  added help invariant + delete cursor tests")
PYEOF
fi

echo
echo "=== cargo check --workspace --all-targets ==="
if ! cargo check --workspace --all-targets 2>&1; then
    echo "Compilation failed"
    exit 1
fi

echo
echo "=== cargo clippy --workspace --all-targets -- -D warnings ==="
if ! cargo clippy --workspace --all-targets -- -D warnings 2>&1; then
    echo "Clippy failed"
    exit 1
fi

echo
if git diff --quiet && git diff --cached --quiet; then
    echo "Nothing to commit — all pieces already present."
    exit 0
fi

echo "Committing."
git add -A
git commit -F - <<'MSG'
fix(tui): sync help text, fix Delete cursor, hide hardware cursor

Three small fixes collected from an interrupted batch.

1. Delete key in insert mode jumped the cursor to the end of the
   input. The TUI built a new string and handed it to set_input,
   which resets cursor_position to input.len() as a side effect.
   Add KodApp::delete_at_cursor, edit in place like backspace does,
   and route the Delete key through it. Also reset completion.

2. The terminal's own cursor was never hidden. InputWidget draws
   an inline block at the current position, so the user saw two
   carets: one at the input box and one wherever ratatui last
   placed the hardware cursor. init_terminal now calls
   terminal.hide_cursor(); restore_terminal's existing
   show_cursor() puts it back on the way out.

3. SLASH_HELP was missing /debug and did not mention that /model
   with no argument lists the server's models. It also repeated
   'j/k scroll' in the Keys line. The idle hint line said
   '/ search', but '/' opens the command slot; the search key is
   'f'. Both rewritten.

Adds four tests: Delete preserves the cursor on a mid-word press,
Backspace still moves the cursor left by one, SLASH_HELP lists
every SLASH_COMMANDS entry (and mentions no unknown command), and
the idle hint names 'f' as the search key.
MSG
