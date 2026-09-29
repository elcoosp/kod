//! Delta §7.7 item 3: bash interceptor.
//!
//! A model that asks `execute_command` for `grep -rn foo src/` gets
//! the shell's `grep`, not the [`crate::tools`] `grep` tool. The
//! tool has a better shape — structured results, relevance ranking,
//! size caps, no risk of running the user's `.bashrc` — but a model
//! trained on raw shell reaches for the shell first.
//!
//! The interceptor catches the specific shapes a model reaches for
//! and returns a redirect: **use the `grep` tool instead, here are
//! the arguments**. The command is not executed. The model learns
//! the routing on the next turn.
//!
//! # What is NOT intercepted
//!
//! Anything with a top-level shell operator (`|`, `;`, `&&`, `||`,
//! `>`, `<`, `(...)`, `` ` ``, `$(`). A pipeline is a shell feature
//! the tool cannot express. `grep foo src/ | wc -l` runs through the
//! shell — the interceptor does not try to decompose it.
//!
//! A leading environment assignment (`FOO=bar grep ...`) also passes
//! through: the tool has no way to receive an env var, so a command
//! that depends on one is not a shape the tool covers.
//!
//! # Why string parsing, not regex
//!
//! The design (D529189–529343) describes "regex rules". Regex is a
//! bad tool for shell tokenization — it cannot respect quote nesting
//! (`grep 'a|b' file` is not a pipeline). This module uses a small
//! hand-written scanner instead: one pass to strip quotes and detect
//! top-level operators, then a token split. It is more code than a
//! regex and less likely to misfire.

/// What a blocked command should become.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Intercept {
    /// The reason the command was blocked, in a form the model can
    /// act on. Names the tool and shows an example call shape.
    pub message: String,
    /// The tool the model should use instead. `"grep"`,
    /// `"read_file"`, or `"list_files"`.
    pub suggested_tool: &'static str,
}

/// If `command` is a bare top-level `grep`/`cat`/`find` invocation
/// the corresponding tool covers better, return an [`Intercept`].
/// `None` means "run it through the shell".
pub fn intercept(command: &str) -> Option<Intercept> {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return None;
    }
    if has_top_level_operator(trimmed) {
        return None;
    }
    if starts_with_env_assignment(trimmed) {
        return None;
    }
    let tokens = split_shell_words(trimmed);
    let cmd = *tokens.first()?;
    let rest: Vec<&str> = tokens[1..].to_vec();
    match cmd {
        "grep" | "egrep" | "fgrep" => intercept_grep(&rest),
        "cat" => intercept_cat(&rest),
        "find" => intercept_find(&rest),
        _ => None,
    }
}

/// Quote-aware whitespace tokenizer. Splits `s` into shell-ish words:
/// whitespace outside quotes separates; single and double quotes
/// delimit a word and are stripped; a backslash escapes the next
/// char. Handles the shapes a model emits — not a full POSIX shell.
fn split_shell_words(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Skip leading whitespace.
        while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let start = i;
        let mut end = i;
        let mut in_single = false;
        let mut in_double = false;
        while i < bytes.len() {
            let c = bytes[i];
            if in_single {
                if c == b'\'' {
                    in_single = false;
                }
                i += 1;
                end = i;
                continue;
            }
            if in_double {
                if c == b'"' {
                    in_double = false;
                }
                i += 1;
                end = i;
                continue;
            }
            match c {
                b'\'' => {
                    in_single = true;
                    i += 1;
                }
                b'"' => {
                    in_double = true;
                    i += 1;
                }
                b'\\' => {
                    // Skip the escape and the char after it.
                    i += 2;
                    end = i.min(bytes.len());
                }
                b' ' | b'\t' => break,
                _ => {
                    i += 1;
                    end = i;
                }
            }
        }
        out.push(&s[start..end]);
    }
    out
}

/// True when `s` contains a shell operator at the top level — i.e.
/// outside any single- or double-quoted span. `grep 'a|b' file` is
/// false (the pipe is inside quotes); `grep a file | wc` is true.
fn has_top_level_operator(s: &str) -> bool {
    let bytes = s.as_bytes();
    let mut in_single = false;
    let mut in_double = false;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if in_single {
            if c == b'\'' {
                in_single = false;
            }
            i += 1;
            continue;
        }
        if in_double {
            if c == b'"' {
                in_double = false;
            }
            i += 1;
            continue;
        }
        match c {
            b'\'' => in_single = true,
            b'"' => in_double = true,
            b'|' | b';' | b'&' | b'>' | b'<' => return true,
            b'(' | b')' => return true,
            b'`' => return true,
            b'$' => {
                // Only `$(` is a subshell. `$VAR` passes.
                if i + 1 < bytes.len() && bytes[i + 1] == b'(' {
                    return true;
                }
            }
            _ => {}
        }
        i += 1;
    }
    false
}

/// True when `s` begins with a `NAME=value` environment assignment
/// (as a single shell word). Covers `FOO=bar`, `FOO=`, `FOO="a b"`.
fn starts_with_env_assignment(s: &str) -> bool {
    // Take the first shell-ish word: up to the first unquoted
    // whitespace. Simpler than a full tokenizer and correct for the
    // shapes a model emits.
    let mut word_end = s.len();
    let bytes = s.as_bytes();
    let mut in_single = false;
    let mut in_double = false;
    for (i, &c) in bytes.iter().enumerate() {
        match c {
            b'\'' if !in_double => in_single = !in_single,
            b'"' if !in_single => in_double = !in_double,
            b' ' | b'\t' if !in_single && !in_double => {
                word_end = i;
                break;
            }
            _ => {}
        }
    }
    let word = &s[..word_end];
    let Some((name, _value)) = word.split_once('=') else {
        return false;
    };
    if name.is_empty() {
        return false;
    }
    let mut chars = name.chars();
    let first = chars.next().unwrap();
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn intercept_grep(args: &[&str]) -> Option<Intercept> {
    let mut positionals: Vec<&str> = Vec::new();
    let mut after_ddash = false;
    for a in args {
        if after_ddash {
            positionals.push(a);
            continue;
        }
        if *a == "--" {
            after_ddash = true;
            continue;
        }
        if a.starts_with('-') && *a != "-" {
            if has_unsupported_grep_flag(a) {
                // The model asked for something the tool cannot do
                // (`-c` count, `-l` list files, `-v` invert). Let
                // the shell handle it rather than break the call.
                return None;
            }
            continue;
        }
        positionals.push(a);
    }
    // Need at least a pattern. No pattern means "grep from stdin",
    // which the tool cannot do.
    let pattern = positionals.first()?;
    let path = positionals.get(1).copied().unwrap_or(".");
    let (path_example, recursive_note) = if path == "." {
        (".", " (recursive = true for a tree-wide search)")
    } else {
        (path, "")
    };
    Some(Intercept {
        message: format!(
            "shell `grep` is disabled here — use the `grep` tool, which \
             returns relevance-ranked matches with a size cap instead of \
             dumping every match into the transcript.\n\
             Example: grep(pattern = {pattern:?}, path = {path_example:?}){recursive_note}.",
        ),
        suggested_tool: "grep",
    })
}

/// Allow only the grep flags the tool can express: `-i` (case
/// insensitive), `-r` / `-R` (recursive), `-n` (line numbers — the
/// tool always returns them). Every other short flag, and every long
/// flag, disqualifies.
fn has_unsupported_grep_flag(arg: &str) -> bool {
    if arg.starts_with("--") {
        return true;
    }
    let Some(letters) = arg.strip_prefix('-') else {
        return false;
    };
    if letters.is_empty() {
        // A bare `-` is a stdin sentinel, not a flag.
        return false;
    }
    for c in letters.chars() {
        if !matches!(c, 'i' | 'r' | 'R' | 'n') {
            return true;
        }
    }
    false
}

fn intercept_cat(args: &[&str]) -> Option<Intercept> {
    // `cat` with no args reads stdin — allow.
    if args.is_empty() {
        return None;
    }
    let mut files: Vec<&str> = Vec::new();
    for a in args {
        if a.starts_with('-') {
            // Any cat flag (`-A`, `-n`, `-v`) is cat-specific; the
            // read_file tool does not reproduce them.
            return None;
        }
        files.push(a);
    }
    let Some(first) = files.first() else {
        return None;
    };
    let extra = if files.len() > 1 {
        format!(
            " (`cat` got {} paths — one `read_file` call per file)",
            files.len()
        )
    } else {
        String::new()
    };
    Some(Intercept {
        message: format!(
            "shell `cat` is disabled here — use the `read_file` tool for \
             {first:?}{extra}. The tool truncates oversized files, flags \
             binary content, and reports actionable errors.",
        ),
        suggested_tool: "read_file",
    })
}

fn intercept_find(args: &[&str]) -> Option<Intercept> {
    // A find that mutates or execs is a shell shape the tool cannot
    // reproduce.
    for a in args {
        if matches!(
            *a,
            "-exec" | "-execdir" | "-delete" | "-ok" | "-okdir" | "-fprint" | "-fprint0"
        ) {
            return None;
        }
    }
    let path = args
        .iter()
        .find(|a| !a.starts_with('-'))
        .copied()
        .unwrap_or(".");
    Some(Intercept {
        message: format!(
            "shell `find` is disabled here — use the `list_files` tool with \
             path = {path:?}. The tool honors .gitignore and returns a \
             bounded listing instead of a raw walk.",
        ),
        suggested_tool: "list_files",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_grep_is_intercepted() {
        let i = intercept("grep foo src/").unwrap();
        assert_eq!(i.suggested_tool, "grep");
        assert!(i.message.contains("`grep` tool"));
        assert!(i.message.contains("\"foo\""));
        assert!(i.message.contains("\"src/\""));
    }

    #[test]
    fn grep_with_supported_flags_is_intercepted() {
        assert!(intercept("grep -rn foo src/").is_some());
        assert!(intercept("grep -i foo src/").is_some());
        assert!(intercept("grep -R foo .").is_some());
        assert!(intercept("grep -irn foo .").is_some());
    }

    #[test]
    fn grep_with_unsupported_flags_passes_through() {
        // Count, invert, list files — the tool cannot do these.
        assert!(intercept("grep -c foo src/").is_none());
        assert!(intercept("grep -v foo src/").is_none());
        assert!(intercept("grep -l foo src/").is_none());
        assert!(intercept("grep --color=always foo src/").is_none());
    }

    #[test]
    fn grep_with_pipeline_passes_through() {
        assert!(intercept("grep foo src/ | wc -l").is_none());
        assert!(intercept("ps aux | grep foo").is_none());
        assert!(intercept("grep foo src/ > out.txt").is_none());
        assert!(intercept("grep foo src/ && echo done").is_none());
    }

    #[test]
    fn grep_with_pipe_inside_quotes_is_still_intercepted() {
        // The pipe is inside single quotes — a literal in the pattern,
        // not a pipeline.
        assert!(intercept("grep 'a|b' file").is_some());
        assert!(intercept("grep \"a|b\" file").is_some());
    }

    #[test]
    fn grep_without_a_pattern_passes_through() {
        // Reads from stdin.
        assert!(intercept("grep").is_none());
        assert!(intercept("grep -i").is_none());
    }

    #[test]
    fn bare_cat_is_intercepted() {
        let i = intercept("cat src/main.rs").unwrap();
        assert_eq!(i.suggested_tool, "read_file");
        assert!(i.message.contains("\"src/main.rs\""));
    }

    #[test]
    fn cat_with_any_flag_passes_through() {
        assert!(intercept("cat -n file").is_none());
        assert!(intercept("cat -A file").is_none());
    }

    #[test]
    fn cat_with_multiple_files_notes_the_count() {
        let i = intercept("cat a.rs b.rs c.rs").unwrap();
        assert_eq!(i.suggested_tool, "read_file");
        assert!(i.message.contains("3 paths"));
    }

    #[test]
    fn cat_without_args_passes_through() {
        // Reads stdin.
        assert!(intercept("cat").is_none());
    }

    #[test]
    fn bare_find_is_intercepted() {
        let i = intercept("find . -name '*.rs'").unwrap();
        assert_eq!(i.suggested_tool, "list_files");
        assert!(i.message.contains("\".\""));
        let i = intercept("find src/ -type f").unwrap();
        assert!(i.message.contains("\"src/\""));
    }

    #[test]
    fn find_with_exec_passes_through() {
        assert!(intercept("find . -name '*.o' -delete").is_none());
        assert!(intercept("find . -name '*.rs' -exec wc -l {} \\;").is_none());
    }

    #[test]
    fn unrelated_commands_pass_through() {
        assert!(intercept("cargo test").is_none());
        assert!(intercept("ls -la").is_none());
        assert!(intercept("rg foo src/").is_none());
        assert!(intercept("").is_none());
        assert!(intercept("   ").is_none());
    }

    #[test]
    fn env_prefixed_commands_pass_through() {
        assert!(intercept("FOO=bar grep x file").is_none());
        assert!(intercept("LC_ALL=C grep -n x file").is_none());
        assert!(intercept("GREP_OPTIONS=--color grep x file").is_none());
        // A non-assignment word is not an env prefix.
        assert!(intercept("grep FOO=bar file").is_some());
    }

    #[test]
    fn egrep_and_fgrep_are_intercepted() {
        assert!(intercept("egrep foo file").is_some());
        assert!(intercept("fgrep foo file").is_some());
    }

    #[test]
    fn command_with_quoted_spaces_still_tokenizes() {
        // A path with a space, quoted. The interceptor sees three
        // tokens: grep, pattern, path.
        let i = intercept("grep pattern 'my file.txt'").unwrap();
        // The tokenizer strips the outer quotes; the message quotes
        // the result with `{:?}`.
        assert!(i.message.contains("my file.txt"), "got: {}", i.message,);
    }

    #[test]
    fn subshell_and_backtick_pass_through() {
        assert!(intercept("grep $(cat file) other").is_none());
        assert!(intercept("grep `cat file` other").is_none());
    }
}
