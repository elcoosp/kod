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

echo "Fixing /theme unknown-name lie; theme hint in help text"

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
# 1. KodApp::set_theme returns the previous name, not needed; add a
#    try_set_theme that reports whether a name matched a real theme.
# ========================================================================
patch(
    app,
    '''    pub fn theme(&self) -> &Theme {
        &self.theme
    }

    pub fn set_theme(&mut self, theme: Theme) {
        self.theme = theme;
    }''',
    '''    pub fn theme(&self) -> &Theme {
        &self.theme
    }

    pub fn set_theme(&mut self, theme: Theme) {
        self.theme = theme;
    }

    /// Set the theme by name, returning `true` if the name matches a
    /// built-in theme (currently `dark` and `light`) and `false`
    /// otherwise. On `false` the theme is set to dark — the same
    /// fallback `Theme::from_name` has always applied — but the caller
    /// can now tell the user that the name was not recognized instead
    /// of printing a transition into a theme that does not exist.
    ///
    /// Previously, `/theme neon` printed "Theme dark → neon" while the
    /// palette was in fact dark; a small lie, but the whole point of
    /// a theme command is to see what you typed take effect.
    pub fn try_set_theme(&mut self, name: &str) -> bool {
        let lower = name.trim().to_ascii_lowercase();
        match lower.as_str() {
            "dark" => {
                self.theme = Theme::dark();
                true
            }
            "light" => {
                self.theme = Theme::light();
                true
            }
            _ => {
                self.theme = Theme::dark();
                false
            }
        }
    }''',
    "try_set_theme",
)

# ========================================================================
# 2. /theme handler uses try_set_theme and reports unknowns honestly.
# ========================================================================
patch(
    loop,
    '''            "/theme" => match parts.next() {
                Some(name) => {
                    let next = Theme::from_name(name);
                    let prev = self.app.theme_name().to_string();
                    self.app.set_theme(next);
                    self.app
                        .push_system_message(&format!("Theme {prev} → {}", name));
                }
                None => {
                    let cur = self.app.theme_name().to_string();
                    let next = self.app.cycle_theme();
                    self.app
                        .push_system_message(&format!("Theme {cur} → {next}"));
                }
            },''',
    '''            "/theme" => match parts.next() {
                Some(name) => {
                    let prev = self.app.theme_name().to_string();
                    let known = self.app.try_set_theme(name);
                    if known {
                        self.app
                            .push_system_message(&format!("Theme {prev} → {name}"));
                    } else {
                        self.app.push_system_message(&format!(
                            "Unknown theme '{}'. Known themes: dark, light. \\
                             Fell back to dark. (Custom themes come from ~/.config/kod/theme.toml \\
                             or a project-local .kod-theme.toml.)",
                            name
                        ));
                    }
                }
                None => {
                    let cur = self.app.theme_name().to_string();
                    let next = self.app.cycle_theme();
                    self.app
                        .push_system_message(&format!("Theme {cur} → {next}"));
                }
            },''',
    "/theme honest about unknown names",
)

# ========================================================================
# 3. /theme handler no longer needs Theme directly — check imports.
# ========================================================================
# Theme is still used by cycle_theme indirectly; but main_loop imports
# Theme only for /theme. Verify by searching the file after edit.
with open(loop, "r") as f:
    loop_src = f.read()
if "Theme::" not in loop_src and "use crate::{" in loop_src and "    theme::Theme," in loop_src:
    patch(
        loop,
        "    theme::Theme,\n",
        "",
        "drop unused Theme import",
    )
    print("Removed unused `use crate::theme::Theme` from main_loop")
else:
    print("Theme import still needed somewhere; leaving it")

# ========================================================================
# 4. Test: unknown theme reports accurately; known theme applies.
# ========================================================================
patch(
    loop,
    '''    /// Delete key (insert mode) must remove the character under the''',
    '''    /// `/theme light` must actually change the palette and report the
    /// transition.
    #[tokio::test]
    async fn test_theme_known_name_applies() {
        let mut tui = TuiLoop::new();
        tui.handle_command("/theme light").await.unwrap();
        assert_eq!(tui.app().theme_name(), "light");
        let last = tui.app().messages().last().unwrap();
        assert!(last.content.contains("→ light"), "got: {}", last.content);
    }

    /// `/theme neon` must not claim to have switched to a theme that
    /// does not exist. Regression: the previous handler printed
    /// "Theme dark → neon" while the palette fell back to dark.
    #[tokio::test]
    async fn test_theme_unknown_name_reports_fallback() {
        let mut tui = TuiLoop::new();
        tui.handle_command("/theme neon").await.unwrap();
        // Palette fell back to dark.
        assert_eq!(tui.app().theme_name(), "dark");
        let last = tui.app().messages().last().unwrap();
        assert!(
            last.content.contains("Unknown theme"),
            "expected an 'Unknown theme' message, got: {}",
            last.content
        );
        assert!(
            last.content.contains("dark, light"),
            "should name the known themes: {}",
            last.content
        );
        assert!(
            !last.content.contains("→ neon"),
            "must not lie about switching to 'neon': {}",
            last.content
        );
    }

    /// Delete key (insert mode) must remove the character under the''',
    "theme tests",
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
git commit -m "fix(tui): /theme stops lying about unknown names

/theme neon printed 'Theme dark → neon' while the palette silently
fell back to dark — Theme::from_name returns dark for anything it
does not recognize. A small lie, but the whole point of a theme
command is to see what you typed take effect; a user typing /theme
neon now learns the name was rejected instead of wondering why the
colors did not change.

Add KodApp::try_set_theme(name) -> bool, which reports whether the
name matched one of the built-in themes (dark, light) before
applying. The /theme handler uses it: on a match it prints the
transition; on a miss it prints the fallback, the known names, and
where custom themes live (~/.config/kod/theme.toml or a project
.kod-theme.toml, both of which Theme::load already reads — that
line was missing from every user-facing hint).

`/theme` with no argument still cycles dark ↔ light and always
succeeds; only the explicit-name path can now be 'unknown'.

Adds two tests: /theme light applies and reports the transition,
/theme neon reports the fallback and does not print a transition
into a theme that does not exist."
