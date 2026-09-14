#!/usr/bin/env bash
set -uo pipefail

COMPILE_OK=true
LOOP=crates/kod-tui/src/main_loop.rs
APP=crates/kod-tui/src/app.rs

for f in "$LOOP" "$APP"; do
    if [ ! -f "$f" ]; then
        echo "ERROR: missing $f — run from the kod workspace root"
        exit 1
    fi
done

echo "Fixing /help text; hint_line search key; invariant test"

python3 - "$LOOP" "$APP" << 'PYEOF'
import os
import sys

loop, app = sys.argv[1], sys.argv[2]

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
# 1. SLASH_HELP: add /debug, /model no-arg hint, fix duplicate "j/k scroll"
# ========================================================================
patch(
    loop,
    '''/// Help text for the `/help` command
const SLASH_HELP: &str = "Commands:\\n/help — show this help\\n/clear — clear chat (asks confirm)\\n/undo — restore last /clear\\n/model <name> — switch model\\n/skills — list loaded skills\\n/goal <text> — set a goal the agent works toward until GOAL MET (/goal clear to stop)\\n/steer <instruction> — redirect the running prompt after its current tool call\\n/cancel — stop the running prompt (also Esc or Ctrl+C while it runs)\\n/compact — compact session history now\\n/retry — resend the last prompt\\n/search [<text>] — search chat (n/N next/prev, Esc clears)\\n/copy — copy last assistant reply to clipboard (also `y`)\\n/theme [dark|light] — cycle or set theme\\n/tools — toggle tool-output visibility (also `t`)\\n/quit — quit kod\\n\\nWhile a prompt runs, typing + Enter steers it (same as /steer).\\nKeys: i insert · j/k scroll · wheel scrolls · q quit · j/k scroll · PgUp/PgDn/Home/End · g/G top/bottom · t toggle tools · o expand · y copy · r retry · u undo · f search · ? help · Esc cancel — hold Option/Shift to select text";''',
    '''/// Help text for the `/help` command.
///
/// Kept in sync with `kod_tui::app::SLASH_COMMANDS` by
/// `test_slash_help_lists_every_command` — adding a command to
/// `SLASH_COMMANDS` without updating this string fails the test, so
/// the help output and the `/` autocomplete cannot drift apart.
const SLASH_HELP: &str = "Commands:\\n/help — show this help\\n/clear — clear chat (asks confirm)\\n/undo — restore last /clear\\n/model [<name>] — switch model; no argument lists the server's models\\n/skills — list loaded skills\\n/goal <text> — set a goal the agent works toward until GOAL MET (/goal clear to stop)\\n/steer <instruction> — redirect the running prompt after its current tool call\\n/cancel — stop the running prompt (also Esc or Ctrl+C while it runs)\\n/compact — compact session history now\\n/retry — resend the last prompt (also `r`)\\n/search [<text>] — search chat (n/N next/prev, Esc clears)\\n/copy — copy last assistant reply to clipboard (also `y`)\\n/theme [dark|light] — cycle or set theme\\n/tools — toggle tool-output visibility (also `t`)\\n/debug last-prompt — write the last prompt sent to the model into ~/.kod/last_prompt.txt\\n/quit — quit kod\\n\\nWhile a prompt runs, typing + Enter steers it (same as /steer).\\nKeys: i insert · j/k or wheel scrolls · q quit · PgUp/PgDn/Home/End · g/G top/bottom · t toggle tools · o expand · y copy · r retry · u undo · f search · ? help · Esc cancel — hold Option/Shift to select text";''',
    "SLASH_HELP update",
)

# ========================================================================
# 2. app.rs hint_line: "/ search" is wrong; the search key is `f`
# ========================================================================
patch(
    app,
    '''        } else {
            "i type · / command · j/k scroll · t tools · / search · ? help · q quit".to_string()
        }''',
    '''        } else {
            // `/ search` used to sit here, but the search key is `f`
            // (SearchPrefix); `/` opens the command slot. Name the
            // actual key so the hint is not a small lie.
            "i type · / command · j/k scroll · t tools · f search · ? help · q quit".to_string()
        }''',
    "hint_line search key",
)

# ========================================================================
# 3. Invariant test in main_loop.rs
# ========================================================================
patch(
    loop,
    '''    /// `/theme light` must actually change the palette and report the''',
    '''    /// Every entry in `SLASH_COMMANDS` must appear in `SLASH_HELP`, so
    /// adding a command to the autocomplete without documenting it
    /// fails this test. The previous SLASH_HELP was missing `/debug`
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
        // Every `/`-leading token inside SLASH_HELP should also be a
        // known command, so a typo'd name does not linger. Split on
        // whitespace, keep tokens starting with '/', strip trailing
        // punctuation from each. Compare against the SLASH_COMMANDS set.
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

    /// The idle hint line must name `f` as the search key, matching the
    /// default keybinding, and must not claim `/` starts a search.
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

    /// `/theme light` must actually change the palette and report the''',
    "help invariant tests",
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
git commit -m "fix(tui): sync /help with the actual command set; correct hint

Three small honesty fixes on user-facing text.

1. SLASH_HELP was missing /debug (which the autocomplete already
   advertised) and did not mention that /model with no argument
   lists the server's models. It also repeated 'j/k scroll' in the
   Keys line — a copy-paste slip that pushed the phrase back where
   'wheel scrolls' already covered it. Rewritten to include every
   current command and to fix the Keys line.

2. The idle hint line said '/ search', but '/' opens the command
   slot; the search key is 'f' (SearchPrefix). A user following the
   hint pressed '/' and got a command input. Fixed to 'f search'.

3. Added test_slash_help_lists_every_command, which asserts each
   entry in SLASH_COMMANDS appears in SLASH_HELP and, in the other
   direction, each '/'-leading token in SLASH_HELP is a known
   command. Adding a command to the autocomplete without updating
   the help now fails the test — the exact drift that let /debug sit
   undocumented. Also asserts the hint line names 'f' as search and
   does not mention '/ search'."
