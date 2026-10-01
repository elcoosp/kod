//! Delta §7.7 item 2: the non-interactive environment for subprocesses.
//!
//! A subprocess spawned by `execute_command` inherits the user's shell
//! environment. That is mostly what a build wants (PATH, HOME,
//! proxies), but several variables make a tool behave as if a human
//! were at a terminal:
//!
//! * `PAGER` / `GIT_PAGER` / `MANPAGER` — a pager tries to own the
//!   tty and blocks; the child's stdout is a pipe, so the pager hangs
//!   or writes escape codes into the captured output.
//! * `GIT_EDITOR` / `EDITOR` / `VISUAL` — `git commit` without `-m`
//!   opens an editor and blocks forever.
//! * `GIT_TERMINAL_PROMPT` / `SSH_ASKPASS` — a credential prompt on a
//!   pipe blocks.
//! * `TERM` / `NO_COLOR` / `CLICOLOR` — colour escape codes in the
//!   captured output.
//! * `npm_config_*` update/audit/fund prompts — interactive npm
//!   questions.
//!
//! [`NON_INTERACTIVE`] is the override map; a caller applies it after
//! inheriting the environment so these win over whatever the user set.

/// Environment overrides applied to every subprocess, after the
/// inherited environment. Each entry is `(name, value)`; a `""` value
/// sets the variable empty (which is what `AWS_PAGER=""` needs to
/// disable the AWS pager).
pub const NON_INTERACTIVE: &[(&str, &str)] = &[
    // Pagers: `cat` is a no-op pager; the empty forms disable a tool's
    // pager entirely.
    ("PAGER", "cat"),
    ("GIT_PAGER", "cat"),
    ("MANPAGER", "cat"),
    ("AWS_PAGER", ""),
    // `less` flags: the classic "quit if one screen, no init, raw
    // control chars" set. `FRX` = quit-if-one-screen, raw, no-init.
    ("LESS", "FRX"),
    // No interactive editor.
    ("GIT_EDITOR", "true"),
    ("EDITOR", "true"),
    ("VISUAL", "true"),
    // No credential prompts.
    ("GIT_TERMINAL_PROMPT", "0"),
    ("SSH_ASKPASS", "false"),
    // Terminal: dumb + no colour.
    ("TERM", "dumb"),
    ("NO_COLOR", "1"),
    ("CLICOLOR", "0"),
    // Python: unbuffered so output arrives as it is produced.
    ("PYTHONUNBUFFERED", "1"),
    // CI signal: several tools skip interactive prompts when set, and
    // a non-interactive child is exactly the "CI" case.
    ("CI", "true"),
    ("AGENT", "1"),
    // npm: accept defaults, no update/fund/audit prompts.
    ("npm_config_yes", "true"),
    ("npm_config_update_notifier", "false"),
    ("npm_config_fund", "false"),
    ("npm_config_audit", "false"),
    ("npm_config_progress", "false"),
    // pnpm: skip the self-update check.
    ("PNPM_DISABLE_SELF_UPDATE_CHECK", "true"),
];

/// Apply [`NON_INTERACTIVE`] to a `Command`. Called after the
/// inherited environment is copied so these values win.
pub fn apply(command: &mut tokio::process::Command) {
    for (k, v) in NON_INTERACTIVE {
        command.env(k, v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_map_disables_the_dangerous_vars() {
        let get = |name: &str| NON_INTERACTIVE.iter().find(|(k, _)| *k == name).map(|(_, v)| *v);
        assert_eq!(get("PAGER"), Some("cat"));
        assert_eq!(get("GIT_PAGER"), Some("cat"));
        assert_eq!(get("GIT_EDITOR"), Some("true"));
        assert_eq!(get("GIT_TERMINAL_PROMPT"), Some("0"));
        assert_eq!(get("NO_COLOR"), Some("1"));
        assert_eq!(get("TERM"), Some("dumb"));
        // AWS_PAGER is the empty-string case.
        assert_eq!(get("AWS_PAGER"), Some(""));
    }

    #[test]
    fn no_duplicate_keys() {
        let mut names: Vec<&str> = NON_INTERACTIVE.iter().map(|(k, _)| *k).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(names.len(), before, "duplicate keys in NON_INTERACTIVE");
    }
}
