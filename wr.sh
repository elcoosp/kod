#!/usr/bin/env bash
set -uo pipefail

LOOP=crates/kod-tui/src/main_loop.rs
APP=crates/kod-tui/src/app.rs

echo "=== Callers of begin_search ==="
grep -rn "begin_search" crates/kod-tui/src/ || echo "  none"

echo
echo "=== Search-type usage ==="
grep -rn "search_type\b" crates/kod-tui/src/ || echo "  none"

echo
echo "=== /search handler ==="
awk '/"\/search" =>/,/^            }/' "$LOOP"

echo
echo "Patching $LOOP"

python3 - "$LOOP" "$APP" << 'PYEOF'
import os
import sys

loop, app = sys.argv[1], sys.argv[2]

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

# ----------------------------------------------------------------------
# 1. /search with no args: prefill the input instead of entering the
#    unreachable begin_search state.
# ----------------------------------------------------------------------
patch(
    loop,
    '''            "/search" => {
                let rest: String = parts.collect::<Vec<_>>().join(" ");
                let rest = rest.trim();
                if rest.is_empty() {
                    self.app.begin_search();
                } else {
                    let n = self.app.set_search(rest);
                    self.app.push_system_message(&format!(
                        "Search: \\"{rest}\\" — {n} match(es). n/N next/prev, Esc clears."
                    ));
                }
            }''',
    '''            "/search" => {
                let rest: String = parts.collect::<Vec<_>>().join(" ");
                let rest = rest.trim();
                if rest.is_empty() {
                    // No query yet. Previously this called
                    // `begin_search`, which sets `search_query =
                    // Some("")` — but nothing routes further typing
                    // into that query (`search_type` has no caller),
                    // and `is_searching()` returns false for the
                    // empty string, so the status bar showed the idle
                    // hint and Escape did not clear the phantom
                    // search. The user typed `/search`, saw no
                    // change, and had to type `/search <text>` to
                    // recover.
                    //
                    // Prefill the input with the command plus a
                    // space and switch to Insert mode, so the user's
                    // next keystrokes land where they belong. Same
                    // shape as the `f` keybinding (SearchPrefix).
                    self.app.set_input("/search ".to_string());
                    self.app.set_input_mode(InputMode::Insert);
                } else {
                    let n = self.app.set_search(rest);
                    self.app.push_system_message(&format!(
                        "Search: \\"{rest}\\" — {n} match(es). n/N next/prev, Esc clears."
                    ));
                }
            }''',
    "/search no-arg prefills input",
)

# ----------------------------------------------------------------------
# 2. Update begin_search's doc to say it is not currently reachable.
# ----------------------------------------------------------------------
patch(
    app,
    '''    /// Enter search mode (status bar shows the editor, `/` typed here).
    /// `query` starts the search immediately; empty query just opens the bar.
    pub fn begin_search(&mut self) {''',
    '''    /// Enter search mode with an empty query.
    ///
    /// **Not currently reachable from any command.** It was the
    /// intended entry point for a type-ahead search bar — the status
    /// widget was to render an editor here and `search_type` was to
    /// feed keystrokes into the query — but the widget only shows the
    /// search bar when `is_searching()` is true, and `is_searching()`
    /// is false for the empty query this sets. `search_type` has no
    /// caller either. The `/search` command handler now prefills the
    /// input box and switches to Insert mode instead.
    ///
    /// Kept as a public method so a future type-ahead implementation
    /// has a starting point. If you wire it up, also make
    /// `is_searching()` true while the query is being edited (or add
    /// a distinct `is_editing_search()`), or the widget will keep
    /// rendering the idle hint — the exact mismatch that made this
    /// unreachable in the first place.
    pub fn begin_search(&mut self) {''',
    "begin_search: document unreachable",
)

# ----------------------------------------------------------------------
# 3. Test.
# ----------------------------------------------------------------------
with open(loop, "r") as f:
    src = f.read()

if "test_search_no_arg_prefills_input" not in src:
    anchor = '''    /// Dispatching a plain prompt must record it as `last_prompt`, so'''
    if anchor not in src:
        print("  ERROR: test anchor not found")
        sys.exit(2)
    new_test = '''    /// `/search` with no argument must prefill the input with the
    /// command plus a space and switch to Insert mode, so the user's
    /// next keystroke becomes part of the query. Regression: the
    /// previous handler called `begin_search`, which set
    /// `search_query = Some("")` — a state no keystroke could reach
    /// (no `search_type` caller, `is_searching()` false for the empty
    /// string), so the command appeared to do nothing.
    #[tokio::test]
    async fn test_search_no_arg_prefills_input() {
        let mut tui = TuiLoop::new();
        tui.handle_command("/search").await.unwrap();
        assert_eq!(
            tui.app().input(),
            "/search ",
            "no-arg /search must prefill the input"
        );
        assert_eq!(
            tui.app().input_mode(),
            &InputMode::Insert,
            "no-arg /search must switch to Insert mode"
        );
        // The phantom-search state must not exist.
        assert!(
            !tui.app().is_searching(),
            "no-arg /search must not enter a search state"
        );
    }

    /// `/search <text>` still works as before: finds matches and
    /// reports the count.
    #[tokio::test]
    async fn test_search_with_arg_still_works() {
        let mut tui = TuiLoop::new();
        for i in 0..3 {
            tui.app_mut().push_system_message(&format!("needle {i}"));
        }
        tui.handle_command("/search needle").await.unwrap();
        let last = tui.app().messages().last().unwrap();
        assert!(
            last.content.contains("3 match"),
            "expected 3 matches: {}",
            last.content
        );
    }

    /// Dispatching a plain prompt must record it as `last_prompt`, so'''
    src = src.replace(anchor, new_test, 1)
    with open(loop, "w") as f:
        f.write(src)
    print("  added search no-arg tests")

print("Done.")
PYEOF

if [ $? -ne 0 ]; then
    echo "ERROR: patching failed"
    exit 1
fi

echo
echo "cargo check --workspace --all-targets 2>&1 | tail -12"
if ! cargo check --workspace --all-targets 2>&1 | tail -12; then
    echo "Compilation failed"
    exit 1
fi

echo
echo "cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -12"
if ! cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -12; then
    echo "Clippy failed"
    exit 1
fi

cat > /tmp/kod_commit_msg.txt <<'MSG'
fix(tui): /search with no argument prefills input instead of a dead state

The /search command's no-argument branch called KodApp::begin_search,
which sets search_query = Some(""). That state is unreachable by any
keystroke:

  - search_type has no caller, so nothing feeds characters into the
    query once the bar "opens";
  - is_searching() returns false for the empty string, so the status
    widget renders the idle hint (not the search bar) and the Escape
    key does not clear the search.

The user typed /search, saw no change, and had to type
/search <text> to recover.

Replace the no-arg branch with the same shape the `f` keybinding
uses: prefill the input with "/search " and switch to Insert mode,
so the user's next keystrokes land inside the command. The
with-argument branch is unchanged.

begin_search is kept as a public method (a future type-ahead
implementation may want it) but its doc now states it is not
currently reachable and names the invariant a future caller would
have to fix: is_searching() must return true while the query is
being edited, or the widget will keep rendering the idle hint.

Adds two tests: /search with no argument prefills the input and
switches to Insert mode without entering a search state; /search
with an argument still reports the match count.
MSG

git add -A
git commit -F /tmp/kod_commit_msg.txt
rm -f /tmp/kod_commit_msg.txt
