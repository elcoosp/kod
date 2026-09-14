#!/usr/bin/env bash
set -uo pipefail

APP=crates/kod-tui/src/app.rs
CHAT=crates/kod-tui/src/ui/chat.rs
TESTS=crates/kod-tui/tests/ui.rs

for f in "$APP" "$CHAT" "$TESTS"; do
    if [ ! -f "$f" ]; then
        echo "ERROR: missing $f"
        exit 1
    fi
done

echo "Patching $APP: public search_target_message_id accessor"
echo "Patching $CHAT: message_line_offsets + search-aware skip_rows"
echo "Patching $TESTS: search-scroll regression test"

python3 - "$APP" "$CHAT" "$TESTS" << 'PYEOF'
import os
import sys

app, chat, tests = sys.argv[1], sys.argv[2], sys.argv[3]

def patch(path, old, new, label, expect=1):
    with open(path, "r") as f:
        src = f.read()
    n = src.count(old)
    if n == 0:
        print(f"  SKIP (anchor absent): {label}")
        return False
    if expect and n != expect:
        print(f"  ERROR: expected {expect} occurrence(s) of {label} in {path}, found {n}")
        sys.exit(2)
    tmp = path + ".tmp"
    with open(tmp, "w") as f:
        f.write(src.replace(old, new, expect if expect else n))
    os.replace(tmp, path)
    print(f"  patched {path}: {label}")
    return True

# ======================================================================
# 1. app.rs: public accessor for the currently targeted match's id.
# ======================================================================
patch(
    app,
    '''    /// Display string for the status bar. Built here so the widget does
    /// not re-implement the match on `SearchStatus` and drift.
    pub fn search_status_label(&self) -> String {''',
    '''    /// The message id currently targeted by the active search, if
    /// any. Returns `None` when no search query is set, when the
    /// query is empty (editing), or when it found no matches.
    ///
    /// Used by the chat widget to scroll the targeted message into
    /// view. The underlying `search_index` is private so the widget
    /// cannot reach it directly; this exposes exactly what the widget
    /// needs (the id of the match to center) without exposing the
    /// index arithmetic.
    pub fn search_target_message_id(&self) -> Option<&kod_types::MessageId> {
        let matches = self.search_matches();
        if matches.is_empty() {
            return None;
        }
        let pos = self.search_index % matches.len();
        let msg_idx = *matches.get(pos)?;
        self.messages().get(msg_idx).map(|m| &m.id)
    }

    /// Display string for the status bar. Built here so the widget does
    /// not re-implement the match on `SearchStatus` and drift.
    pub fn search_status_label(&self) -> String {''',
    "search_target_message_id",
)

# ======================================================================
# 2. chat.rs: declare the offsets map, record offsets, override skip_rows.
# ======================================================================
patch(
    chat,
    '''        let mut lines: Vec<Line> = Vec::new();
        // A dim rule between turns (never after the last one) so exchanges
        // scan visually instead of piling up with one blank row.
        let mut hidden_tools = 0;
        for (i, message) in ordered.iter().enumerate() {''',
    '''        let mut lines: Vec<Line> = Vec::new();
        // A dim rule between turns (never after the last one) so exchanges
        // scan visually instead of piling up with one blank row.
        let mut hidden_tools = 0;
        // Maps each rendered message's id to the index of its first
        // line in `lines`. Used by the search-active scroll override
        // below to compute the target message's vertical position.
        let mut message_line_offsets: std::collections::HashMap<
            kod_types::MessageId,
            usize,
        > = std::collections::HashMap::new();
        for (i, message) in ordered.iter().enumerate() {''',
    "declare message_line_offsets",
)

patch(
    chat,
    '''            if i > 0 && lines.last().map(|l| l.width()).unwrap_or(1) > 0 {
                lines.push(Line::from(vec![Span::styled(
                    "─".repeat(text_width.min(120)),
                    Style::default().fg(theme.dim),
                )]));
            }
            lines.extend(Self::message_lines(app, message, text_width));
        }''',
    '''            if i > 0 && lines.last().map(|l| l.width()).unwrap_or(1) > 0 {
                lines.push(Line::from(vec![Span::styled(
                    "─".repeat(text_width.min(120)),
                    Style::default().fg(theme.dim),
                )]));
            }
            message_line_offsets.insert(message.id.clone(), lines.len());
            lines.extend(Self::message_lines(app, message, text_width));
        }''',
    "record per-message line offsets",
)

patch(
    chat,
    '''        let max_offset = total_rows.saturating_sub(height);
        let offset = app.scroll_offset().min(max_offset);
        let skip_rows = total_rows
            .saturating_sub(height)
            .saturating_sub(offset)
            .min(u16::MAX as usize) as u16;''',
    '''        let max_offset = total_rows.saturating_sub(height);
        let offset = app.scroll_offset().min(max_offset);
        // Skip-to-row for this frame. While a search is active, the
        // targeted message is centered in the viewport — that is the
        // user's focus, not wherever they had scrolled to before
        // starting the search. The user's own scroll offset
        // (`app.scroll_lines`) is never touched, so clearing the
        // search (Escape) or the search finding no more matches
        // restores the previous viewport immediately.
        //
        // The centering computation is a per-frame cost only while a
        // search is active: measure the visual rows of the `lines`
        // prefix before the target, then subtract half the viewport.
        // `line_count` gives the true wrapped-row count, matching the
        // scrollbar's own measurement a few lines down.
        let skip_rows = if let Some(target_id) = app.search_target_message_id() {
            if let Some(&line_idx) = message_line_offsets.get(target_id) {
                let prefix = Text::from(lines[..line_idx].to_vec());
                #[allow(unstable_name_collisions)]
                let prefix_rows = Paragraph::new(prefix)
                    .wrap(Wrap { trim: false })
                    .line_count(text_width as u16) as usize;
                let desired = prefix_rows.saturating_sub(height / 2);
                desired.min(max_offset).min(u16::MAX as usize) as u16
            } else {
                // Target message did not render in this frame (it was
                // a tool row and `show_tools` is off, for example).
                // Fall back to the user's own scroll.
                total_rows
                    .saturating_sub(height)
                    .saturating_sub(offset)
                    .min(u16::MAX as usize) as u16
            }
        } else {
            total_rows
                .saturating_sub(height)
                .saturating_sub(offset)
                .min(u16::MAX as usize) as u16
        };''',
    "search-aware skip_rows override",
)

# ======================================================================
# 3. ui.rs test.
# ======================================================================
with open(tests, "r") as f:
    tests_src = f.read()

if "test_search_scrolls_target_into_view" not in tests_src:
    anchor = '''#[test]
fn test_scroll_to_top_reaches_oldest() {'''
    if anchor not in tests_src:
        # Try an alternative: any function that starts the chat tests.
        print("  WARN: test anchor not found; appending test at end of file")
        # Append at end. Assume `use` statements are already present.
        addition = '''

/// With an active search targeting an early message, the chat widget
/// must scroll that message into view — not stay pinned to the live
/// bottom where the user happened to be before searching.
#[test]
fn test_search_scrolls_target_into_view() {
    use kod_tui::app::KodApp;
    use kod_tui::ui::ChatWidget;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let mut app = KodApp::new();
    // Enough messages that the first one is far off-screen if the
    // viewport were pinned to the bottom.
    for i in 0..40 {
        app.push_system_message(&format!("line-{i}-{}", "x".repeat(60)));
    }
    // A search whose only hit is the very first message. `line-0-`
    // also matches `line-0` when padded the way we padded it — the
    // assertion below scans for the exact text, so a false positive
    // from `line-0` in `line-0X` is not possible; the strings differ
    // at the 6th character.
    let n = app.set_search("line-0-x");
    assert_eq!(n, 1, "search should find exactly the first message");
    assert!(app.is_searching());

    let backend = TestBackend::new(80, 8);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|f| {
            ChatWidget::new().render(&app, f.area(), f.buffer_mut());
        })
        .unwrap();

    let visible: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    assert!(
        visible.contains("line-0-x"),
        "search target should be visible; got:\\n{visible}"
    );
}
'''
        with open(tests, "a") as f:
            f.write(addition)
        print("  appended search-scroll test at file end")
    else:
        addition = '''/// With an active search targeting an early message, the chat widget
/// must scroll that message into view — not stay pinned to the live
/// bottom where the user happened to be before searching.
#[test]
fn test_search_scrolls_target_into_view() {
    use kod_tui::app::KodApp;
    use kod_tui::ui::ChatWidget;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let mut app = KodApp::new();
    for i in 0..40 {
        app.push_system_message(&format!("line-{i}-{}", "x".repeat(60)));
    }
    let n = app.set_search("line-0-x");
    assert_eq!(n, 1, "search should find exactly the first message");
    assert!(app.is_searching());

    let backend = TestBackend::new(80, 8);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|f| {
            ChatWidget::new().render(&app, f.area(), f.buffer_mut());
        })
        .unwrap();

    let visible: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    assert!(
        visible.contains("line-0-x"),
        "search target should be visible; got:\\n{visible}"
    );
}

'''
        with open(tests, "w") as f:
            f.write(tests_src.replace(anchor, addition + anchor, 1))
        print("  inserted search-scroll test before test_scroll_to_top_reaches_oldest")

print("Done.")
PYEOF

if [ $? -ne 0 ]; then
    echo "ERROR: patching failed"
    exit 1
fi

echo
echo "cargo check --workspace --all-targets 2>&1 | tail -15"
if ! cargo check --workspace --all-targets 2>&1 | tail -15; then
    echo "Compilation failed"
    exit 1
fi

echo
echo "cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -15"
if ! cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -15; then
    echo "Clippy failed"
    exit 1
fi

cat > /tmp/kod_commit_msg.txt <<'MSG'
feat(tui): scroll the chat to a search match

KodApp::jump_to_search_match called scroll_to_bottom() and its
comment claimed "the viewport math lives in the chat widget, which
centers matches when a search is active." That was a comment about
intent, not the implementation: the widget never centered matches.
The user ran /search foo, the status bar reported "1/3", and the
viewport stayed wherever it was. If the target was off-screen, the
highlight the widget did draw was invisible.

Add KodApp::search_target_message_id() — the id of the message
currently targeted by the active search, or None when no search,
empty query, or no matches. The chat widget records each rendered
message's starting line index during the message-building loop,
keys those offsets by message id, and — while a search is active —
overrides skip_rows to center the target:

  prefix = lines[..target_start]
  target_row = Paragraph(prefix).line_count(text_width)
  skip = (target_row - height/2).min(max_offset)

The override is per-frame: `app.scroll_lines` is not touched, so
pressing Escape (clear_search) or exhausting the matches restores
the user's previous viewport immediately. That is the behaviour a
user expects from a search — "show me the match, then get out of
the way when I'm done."

Cost: the prefix `line_count` runs once per render frame while a
search is active. That is O(lines) work, negligible next to the
frame's existing O(lines) row measurement for the scrollbar (which
runs unconditionally).

If the target message did not render in the current frame (it was a
tool row and `show_tools` is off), the widget falls back to the
user's scroll offset rather than jumping to a nonexistent row.

Adds test_search_scrolls_target_into_view: 40 filler messages, a
search matching only the first, a small TestBackend viewport, and
an assertion that the target's text appears in the rendered buffer.
MSG

git add -A
git commit -F /tmp/kod_commit_msg.txt
rm -f /tmp/kod_commit_msg.txt
