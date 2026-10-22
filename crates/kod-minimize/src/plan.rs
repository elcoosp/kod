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
    if trimmed.contains("<<") || trimmed.starts_with('(') || trimmed.starts_with('{') {
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
    // T5-C35: strip matched surrounding quotes so `git "status"` matches
    // a def keyed on `status`.
    let args: Vec<String> = tokens.map(|t| strip_quotes(t).to_string()).collect();
    CommandPlan::Single {
        program: program.to_string(),
        args,
    }
}

/// T5-C35: a token like `'status'` or `"status"` should compare equal to
/// the unquoted form. Strip matched surrounding quotes.
fn strip_quotes(tok: &str) -> &str {
    let b = tok.as_bytes();
    if b.len() >= 2
        && ((b[0] == b'\'' && b[b.len() - 1] == b'\'') || (b[0] == b'"' && b[b.len() - 1] == b'"'))
    {
        &tok[1..tok.len() - 1]
    } else {
        tok
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

    #[test]
    fn strip_quotes_strips_only_matched_pairs() {
        assert_eq!(strip_quotes("abc"), "abc");
        assert_eq!(strip_quotes("'abc'"), "abc");
        assert_eq!(strip_quotes("\"abc\""), "abc");
        assert_eq!(strip_quotes("'"), "'");
        assert_eq!(strip_quotes(""), "");
        assert_eq!(strip_quotes("'abc"), "'abc");
        assert_eq!(strip_quotes("abc'"), "abc'");
        assert_eq!(strip_quotes("'abc\""), "'abc\"");
    }
}

#[cfg(test)]
mod proptest_classify {
    //! Property tests for `classify`'s safety and completeness
    //! contracts.
    //!
    //! The module doc states the classifier's safety claim directly:
    //!
    //! > False negatives (a `Single` that is actually a pipe) are
    //! > impossible because the scan is exhaustive on the operator
    //! > set.
    //!
    //! That claim is exactly what a property test can prove
    //! non-vacuously. The example tests above pin one operator case
    //! each (bare pipe, quoted pipe, escaped pipe, `&&`, `;`,
    //! background `&`, leading operator, here-doc, subshell, brace
    //! compound, unbalanced quote). What they do not prove is that
    //! the *classification* is complete and safe across generated
    //! inputs — the claims by name, verified example-only, that
    //! session 15's P2 hid in.
    //!
    //! Session 18 addition. Three of the four properties assert on
    //! non-empty `Single` outputs; the fourth asserts on operator
    //! inputs. Every property is non-vacuous by construction (no
    //! `prop_assume!` over a wide domain — the `prop::sample::select`
    //! and `prop_filter` generators from the previous rounds'
    //! lessons).

    use super::*;
    use proptest::prelude::*;

    /// Arbitrary ASCII. Used for the totality and safety properties
    /// (which fire on most inputs).
    fn any_cmd() -> impl Strategy<Value = String> {
        ".{0,200}"
    }

    /// A command with no quote characters. Used by the safety
    /// property: if `c` has no `'`, `"`, or `\`, then "outside quotes"
    /// is "the whole string," and the assertion "no `| & ;`" is
    /// unambiguous.
    fn quote_free_cmd() -> impl Strategy<Value = String> {
        ".{0,200}".prop_filter("no quote or escape chars", |s| {
            !s.contains('\'') && !s.contains('"') && !s.contains('\\')
        })
    }

    /// A clean command: no operator chars, no quote chars, non-empty
    /// after trim. Used by the completeness property: for any such
    /// input, the classifier must return `Single`.
    fn clean_cmd() -> impl Strategy<Value = String> {
        ".{0,100}".prop_filter("clean input", |s| {
            !s.trim().is_empty() && !s.chars().any(|c| "|&;<>(){}\'\"\\".contains(c))
        })
    }

    proptest! {
        /// `classify` never panics on arbitrary input.
        #[test]
        fn prop_classify_never_panics(c in any_cmd()) {
            let _ = classify(&c);
        }

        /// `classify` always returns one of the four documented
        /// variants. Tautological given the enum, but it pins the
        /// totality of the classifier (no `unreachable!`, no
        /// fall-through panic).
        #[test]
        fn prop_classify_returns_a_variant(c in any_cmd()) {
            let plan = classify(&c);
            let _ = match plan {
                CommandPlan::Single { .. }
                | CommandPlan::Piped
                | CommandPlan::Chain
                | CommandPlan::Unsupported => true,
            };
        }

        /// **Conservatism** (non-vacuous). For any `a`, `b`
        /// alphanumeric and any shell operator `op`, `classify(a op b)`
        /// is NOT `Single`. The construct guarantees the assertion
        /// fires; no filter, no reject budget.
        ///
        /// This is the doc's "false negatives are impossible" claim in
        /// its sharpest form. A refactor that dropped a `|`, `&&`, `;`,
        /// or `&` case from the classifier would let a real pipeline
        /// through as `Single` — the shape session 15's P2 hid in for
        /// a different class.
        #[test]
        fn prop_operator_implies_not_single(
            a in "[a-zA-Z0-9 ]{0,15}",
            b in "[a-zA-Z0-9 ]{0,15}",
            op in prop::sample::select(vec!["|", "&&", ";", "&"]),
        ) {
            let cmd = format!("{a}{op}{b}");
            let plan = classify(&cmd);
            prop_assert!(
                !matches!(plan, CommandPlan::Single { .. }),
                "operator {:?} in {:?} produced Single {:?} — a real \
                 pipeline would be rewritten as a single command",
                op, cmd, plan,
            );
        }

        /// **Safety** (the doc's claim, general form). If `classify`
        /// returns `Single`, the input contains no top-level
        /// `|`, `&`, or `;`. For quote-free input, "top-level" is
        /// "anywhere in the string."
        ///
        /// Non-vacuity note: most arbitrary inputs are clean and
        /// classify as `Single`, so the assertion fires on the
        /// majority. The `prop_operator_implies_not_single` property
        /// above covers the operator inputs that skip this one.
        #[test]
        fn prop_single_implies_no_operator(c in quote_free_cmd()) {
            let plan = classify(&c);
            if matches!(plan, CommandPlan::Single { .. }) {
                prop_assert!(
                    !c.contains('|') && !c.contains('&') && !c.contains(';'),
                    "Single classified for quote-free input {:?} \
                     containing an operator",
                    c,
                );
            }
        }

        /// **Completeness**. For input with no operator, quote, or
        /// escape chars, and non-empty after trim, `classify` returns
        /// `Single`. This is the inverse direction: the classifier
        /// must not be *too* conservative on clean input.
        #[test]
        fn prop_clean_input_is_single(c in clean_cmd()) {
            let plan = classify(&c);
            prop_assert!(
                matches!(plan, CommandPlan::Single { .. }),
                "clean input {:?} classified as {:?}, expected Single",
                c, plan,
            );
        }
    }
}
