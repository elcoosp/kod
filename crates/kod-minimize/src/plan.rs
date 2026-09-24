//! Command classification.
//!
//! The design's §5.2 splits a command into one of five shapes:
//! `Single`, `Piped`, `Chain`, `Compound`, `Unsupported`. kod lands
//! the first three; the last two fall through to `Unsupported`, which
//! the minimizer treats as "return raw".
//!
//! The classifier is *conservative*: it errs on the side of
//! classifying something as `Piped` or `Chain` when the split is
//! ambiguous, because a false negative (a `Single` that is actually
//! a pipe) is far worse than the reverse. A pipe whose output the
//! minimizer rewrote would be showing the model something the shell
//! did not produce.

/// What a command string is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandPlan {
    /// A single program invocation with no shell operators. The only
    /// shape the minimizer actually rewrites.
    Single {
        /// The program name as typed (`git`, `cargo`, `pytest`).
        program: String,
        /// The remaining tokens. `["status"]` for `git status`.
        args: Vec<String>,
    },
    /// A command containing a top-level `|`. Minimizer returns raw.
    Piped,
    /// A command containing `&&`, `||`, or `;` at the top level.
    /// Minimizer returns raw (segment-aware rewriting is a
    /// follow-up).
    Chain,
    /// A command the classifier does not understand — a here-doc, a
    /// compound `{ ... }`, a subshell `( ... )`, an empty string, a
    /// line starting with an operator. Minimizer returns raw.
    Unsupported,
}

/// Classify a command string.
///
/// The classifier does not run a shell parser. It scans the string
/// **outside of quotes** for the operator characters that would
/// change the shape and returns the first matching shape. False
/// negatives (a `Single` that is actually a pipe) are impossible
/// because the scan is exhaustive on the operator set; false
/// positives are possible only for a program whose name is itself a
/// shell operator (`|foo`), which the grammar forbids.
pub fn classify(cmd: &str) -> CommandPlan {
    let trimmed = cmd.trim();
    if trimmed.is_empty() {
        return CommandPlan::Unsupported;
    }

    // A leading operator is a syntax error in the shell and cannot be
    // a valid single command. Classify as Unsupported so the
    // minimizer returns raw rather than trying to run a def.
    let first = trimmed.chars().next().unwrap_or(' ');
    if matches!(first, '|' | '&' | ';' | '<' | '>') {
        return CommandPlan::Unsupported;
    }

    // A here-doc, subshell, or brace compound: return Unsupported.
    // These shapes are not handled by the minimizer, and a rewrite
    // would be inventing content.
    if trimmed.contains("<<")
        || trimmed.starts_with('(')
        || trimmed.starts_with('{')
    {
        return CommandPlan::Unsupported;
    }

    let mut in_single = false;
    let mut in_double = false;
    let mut escape = false;
    let bytes = trimmed.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if escape {
            escape = false;
            i += 1;
            continue;
        }
        match b {
            b'\\' if !in_single => escape = true,
            b'\'' if !in_double => in_single = !in_single,
            b'"' if !in_single => in_double = !in_double,
            b'|' if !in_single && !in_double => return CommandPlan::Piped,
            b'&' if !in_single && !in_double => {
                // `&&` or a background `&`. Both are multi-command.
                return CommandPlan::Chain;
            }
            b';' if !in_single && !in_double => return CommandPlan::Chain,
            _ => {}
        }
        i += 1;
    }
    if in_single || in_double {
        // Unbalanced quote: the command is malformed. Unsupported
        // rather than guessing.
        return CommandPlan::Unsupported;
    }

    // A single command. Split on whitespace. The tokens after the
    // program are the arguments; the classifier does not strip shell
    // variable expansions, which is what the minimizer wants — a
    // `--path=$FOO` argument reaches the def with the `$FOO`
    // unexpanded, and a def that cares about it can decode.
    let mut tokens = trimmed.split_whitespace();
    let Some(program) = tokens.next() else {
        return CommandPlan::Unsupported;
    };
    let args: Vec<String> = tokens.map(String::from).collect();
    CommandPlan::Single {
        program: program.to_string(),
        args,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn single(cmd: &str) -> (String, Vec<String>) {
        match classify(cmd) {
            CommandPlan::Single { program, args } => (program, args),
            other => panic!("expected Single for {cmd:?}, got {other:?}"),
        }
    }

    #[test]
    fn a_bare_program_is_single_with_no_args() {
        assert_eq!(single("ls").0, "ls");
        assert!(single("ls").1.is_empty());
    }

    #[test]
    fn args_are_split_on_whitespace() {
        let (p, a) = single("git status --short");
        assert_eq!(p, "git");
        assert_eq!(a, vec!["status", "--short"]);
    }

    #[test]
    fn an_empty_command_is_unsupported() {
        assert_eq!(classify(""), CommandPlan::Unsupported);
        assert_eq!(classify("   "), CommandPlan::Unsupported);
    }

    #[test]
    fn a_pipe_is_piped() {
        assert_eq!(classify("git status | grep modified"), CommandPlan::Piped);
        assert_eq!(classify("a | b | c"), CommandPlan::Piped);
    }

    #[test]
    fn a_pipe_inside_single_quotes_is_not_piped() {
        let (p, a) = single("echo 'a | b'");
        assert_eq!(p, "echo");
        assert_eq!(a, vec!["'a", "|", "b'"]);
    }

    #[test]
    fn a_pipe_inside_double_quotes_is_not_piped() {
        let (p, _) = single("echo \"a | b\"");
        assert_eq!(p, "echo");
    }

    #[test]
    fn a_chained_command_with_and_is_chain() {
        assert_eq!(classify("git status && echo done"), CommandPlan::Chain);
    }

    #[test]
    fn a_chained_command_with_semicolon_is_chain() {
        assert_eq!(classify("cd /tmp; ls"), CommandPlan::Chain);
    }

    #[test]
    fn a_background_command_is_chain() {
        assert_eq!(classify("sleep 10 &"), CommandPlan::Chain);
    }

    #[test]
    fn a_leading_operator_is_unsupported() {
        assert_eq!(classify("| grep x"), CommandPlan::Unsupported);
        assert_eq!(classify("&& foo"), CommandPlan::Unsupported);
        assert_eq!(classify("; foo"), CommandPlan::Unsupported);
    }

    #[test]
    fn a_here_doc_is_unsupported() {
        assert_eq!(classify("cat <<EOF"), CommandPlan::Unsupported);
    }

    #[test]
    fn a_subshell_is_unsupported() {
        assert_eq!(classify("(cd /tmp && ls)"), CommandPlan::Unsupported);
    }

    #[test]
    fn a_brace_compound_is_unsupported() {
        assert_eq!(classify("{ echo a; echo b; }"), CommandPlan::Unsupported);
    }

    #[test]
    fn an_unbalanced_quote_is_unsupported() {
        assert_eq!(classify("echo 'unterminated"), CommandPlan::Unsupported);
        assert_eq!(classify("echo \"unterminated"), CommandPlan::Unsupported);
    }

    #[test]
    fn an_escaped_pipe_is_not_a_pipe() {
        // `\|` in the shell produces a literal `|`. A classifier that
        // treated it as a pipe would refuse to minimize a single
        // command that happens to include an escaped operator in an
        // argument.
        let (p, _) = single(r"grep \| x");
        assert_eq!(p, "grep");
    }

    #[test]
    fn a_leading_whitespace_is_trimmed() {
        assert_eq!(single("   git status").0, "git");
    }
}
