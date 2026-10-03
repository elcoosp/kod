//! Command classification: tokenize, unwrap, score.
//!
//! The scorer reads a shell command the way a shell would — through
//! the wrappers that hide the real program (`sudo`, `env`, `xargs`),
//! past the redirections that decide what gets truncated, and into
//! the arguments that name paths. It classifies by *blast radius*,
//! not by command name: `rm` on a scratch file is Low, `rm` on the
//! home directory is Catastrophic, and the name `rm` alone tells you
//! neither.

use crate::paths::classify as classify_path;
use crate::paths::{PathDanger, RiskContext};

/// How dangerous a command is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RiskLevel {
    Safe,
    Low,
    Confirm,
    Catastrophic,
}

impl RiskLevel {
    /// Whether the command may run without asking anything.
    pub fn runs_immediately(self) -> bool {
        matches!(self, Self::Safe | Self::Low)
    }
    /// Whether the command is refused outright, no reflection.
    pub fn is_absolute_deny(self) -> bool {
        self == Self::Catastrophic
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Safe => "safe",
            Self::Low => "low",
            Self::Confirm => "confirm",
            Self::Catastrophic => "catastrophic",
        }
    }
}

/// One reason a command scored as it did. `reason` is shown to the
/// model verbatim in a reflection prompt, so it names the path and
/// the operation, not a rule id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RiskFinding {
    pub level: RiskLevel,
    pub reason: String,
    pub target: String,
}

/// The whole classification. `level` is the max of the findings.
#[derive(Debug, Clone)]
pub struct RiskAssessment {
    pub level: RiskLevel,
    pub findings: Vec<RiskFinding>,
}

/// What the caller should do with a command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateOutcome {
    /// Run it.
    Allow,
    /// The model must re-issue with a substantive `justification`.
    Reflect { prompt: String },
    /// Refused; the user must run it themselves.
    Deny { reason: String },
}

/// A justification supplied on a re-issue. Substantive means long
/// enough to be an actual sentence and not an affirmation.
#[derive(Debug, Clone)]
pub struct Justification(pub String);

impl Justification {
    pub fn is_substantive(&self) -> bool {
        let t = self.0.trim();
        if t.chars().count() < 25 {
            return false;
        }
        let lower = t.to_ascii_lowercase();
        !matches!(
            lower.as_str(),
            "yes" | "ok" | "sure" | "proceed" | "confirmed" | "do it" | "go ahead"
        )
    }
}

/// Wrappers that run another program, with the flags that consume a
/// following argument. A wrapper not in this table stops the unwrap —
/// the classifier cannot see through it, which is a `Confirm`.
const WRAPPERS: &[(&str, &[&str])] = &[
    ("sudo", &["-u", "-g", "-p", "-h", "-r", "-t"]),
    ("doas", &["-u"]),
    ("env", &["-u", "-i", "-C", "-S"]),
    ("nice", &["-n"]),
    ("timeout", &["-s", "-k"]),
    ("stdbuf", &["-i", "-o", "-e"]),
    ("ionice", &["-c", "-n", "-p"]),
    ("time", &[]),
    ("command", &[]),
    ("builtin", &[]),
    ("exec", &["-a", "-c"]),
    ("xargs", &["-n", "-I", "-i", "-P", "-s", "-d", "-a", "-E"]),
];

/// Commands that delete, and whose operand paths must be checked.
const DESTRUCTIVE: &[&str] = &[
    "rm", "rmdir", "mv", "dd", "shred", "truncate", "unlink", "cp",
];

/// Commands that write via a mechanism the path check cannot see
/// (`dd` counts, `tee` writes wherever it is pointed).
const DEVICE_CAPABLE: &[&str] = &["dd", "mkfs", "mkswap", "fdisk", "parted", "wipefs"];

/// Tokenize a shell command, honoring single and double quotes and
/// keeping `$VAR` intact (an unexpanded variable is a path the
/// classifier cannot clear, so it must survive tokenization).
///
/// A heredoc body is dropped: a `cat <<EOF` whose text happens to
/// contain the word `rm` must not trip the gate. The parser detects
/// `<<` and skips to the terminator line.
pub fn tokenize(command: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut heredoc_terminator: Option<String> = None;

    let chars: Vec<char> = command.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];

        if let Some(term) = &heredoc_terminator {
            // Inside a heredoc body: skip whole lines until the
            // terminator.
            let line_end = command[..]
                .char_indices()
                .skip(i)
                .find(|(_, ch)| *ch == '\n')
                .map(|(idx, _)| idx)
                .unwrap_or(command.len());
            let _ = line_end;
            let rest: String = chars[i..].iter().collect();
            let mut lines = rest.splitn(2, '\n');
            let this_line = lines.next().unwrap_or("");
            if this_line.trim() == term.as_str() {
                heredoc_terminator = None;
                // advance past this line's newline
                i += this_line.chars().count() + 1;
                continue;
            }
            i += this_line.chars().count() + 1;
            continue;
        }

        match c {
            '\'' if !in_double => {
                in_single = !in_single;
                current.push(c);
            }
            '"' if !in_single => {
                in_double = !in_double;
                current.push(c);
            }
            ' ' | '\t' if !in_single && !in_double => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            '\n' if !in_single && !in_double => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            '<' if !in_single && !in_double && i + 1 < chars.len() && chars[i + 1] == '<' => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
                // Heredoc: the next token is the terminator.
                i += 2;
                // skip optional `-`
                if i < chars.len() && chars[i] == '-' {
                    i += 1;
                }
                while i < chars.len() && chars[i].is_whitespace() {
                    i += 1;
                }
                let mut term = String::new();
                while i < chars.len() && !chars[i].is_whitespace() {
                    term.push(chars[i]);
                    i += 1;
                }
                if !term.is_empty() {
                    heredoc_terminator = Some(
                        term.trim_matches(|c| c == '\'' || c == '"').to_string(),
                    );
                }
                continue;
            }
            c if !in_single && !in_double && matches!(c, '|' | ';' | '&') => {
                // An unquoted operator is its own token, spaced or not.
                // The old fall-through glued `x|rm` into one token, so
                // split_segments never saw the pipe and the destructive
                // half ran unclassified.
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
                if (c == '&' || c == '|')
                    && i + 1 < chars.len()
                    && chars[i + 1] == c
                {
                    tokens.push(format!("{c}{c}"));
                    i += 1;
                } else {
                    tokens.push(c.to_string());
                }
            }
            _ => current.push(c),
        }
        i += 1;
    }
    // T5-C2: if the loop exited while a heredoc was still open, the
    // input had no closing terminator. Everything after `<<EOF` was
    // swallowed as heredoc data, including any command on the same
    // line. Emit a sentinel so `assess` escalates.
    if heredoc_terminator.is_some() {
        tokens.push("__kod_heredoc_unterminated__".to_string());
    }

    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// Strip quotes from a token.
fn unquote(t: &str) -> String {
    let t = t.trim();
    if (t.starts_with('"') && t.ends_with('"') && t.len() >= 2)
        || (t.starts_with('\'') && t.ends_with('\'') && t.len() >= 2)
    {
        t[1..t.len() - 1].to_string()
    } else {
        t.to_string()
    }
}

/// Unwrap a command through its leading wrappers, returning the
/// effective program and its argument slice, plus whether the unwrap
/// stopped cleanly (false means it hit something it could not see
/// through, which escalates).
fn unwrap(tokens: &[String]) -> (Option<String>, Vec<String>, bool) {
    let mut i = 0;
    loop {
        if i >= tokens.len() {
            return (None, Vec::new(), true);
        }
        let tok = unquote(&tokens[i]);
        let Some((_, flag_args)) = WRAPPERS.iter().find(|(w, _)| *w == tok) else {
            // Not a wrapper: this is the program.
            return (Some(tok), tokens[i + 1..].to_vec(), true);
        };
        // Consume the wrapper's own flags until the program.
        i += 1;
        while i < tokens.len() {
            let a = unquote(&tokens[i]);
            if a.starts_with('-') {
                // Does this flag consume the next token?
                let consumes = flag_args.iter().any(|f| a == *f);
                i += 1;
                if consumes {
                    i += 1;
                }
            } else {
                break;
            }
        }
        // For `env -S`, the rest of the line is a single quoted
        // command; we cannot see through it.
        if let Some(pos) = tokens.iter().take(i).position(|t| unquote(t) == "-S") {
            if pos < i {
                return (None, Vec::new(), false);
            }
        }
    }
}

/// Split a command on pipes and semicolons into independent segments.
fn split_segments(tokens: &[String]) -> Vec<Vec<String>> {
    let mut out = vec![Vec::new()];
    for t in tokens {
        let u = unquote(t);
        if u == "|" || u == ";" || u == "&&" || u == "||" {
            out.push(Vec::new());
        } else {
            out.last_mut().unwrap().push(t.clone());
        }
    }
    out.retain(|s| !s.is_empty());
    out
}

/// Assess a command.
pub fn assess(command: &str, ctx: &RiskContext) -> RiskAssessment {
    // T5-C2: `tokenize` emits a sentinel when a heredoc body has no
    // closing terminator (the rest of the command would otherwise be
    // silently swallowed as heredoc data). Escalate so the gate does
    // not return Safe for `cat foo <<EOF; rm -rf /`.
    if tokenize(command)
        .iter()
        .any(|t| t == "__kod_heredoc_unterminated__")
    {
        return RiskAssessment {
            level: RiskLevel::Confirm,
            findings: vec![RiskFinding {
                level: RiskLevel::Confirm,
                reason: "heredoc body had no closing terminator".to_string(),
                target: command.to_string(),
            }],
        };
    }

    let tokens = tokenize(command);
    let mut findings: Vec<RiskFinding> = Vec::new();

    if tokens.is_empty() {
        return RiskAssessment {
            level: RiskLevel::Safe,
            findings,
        };
    }

    let segments = split_segments(&tokens);
    let pipe_fed = segments.len() > 1;

    for seg in &segments {
        let (program, args, clean) = unwrap(seg);

        if !clean {
            findings.push(RiskFinding {
                level: RiskLevel::Confirm,
                reason: "command runs through a wrapper that cannot be inspected statically"
                    .to_string(),
                target: seg.join(" "),
            });
            continue;
        }

        let Some(prog) = program else { continue };
        let prog_base = prog.rsplit('/').next().unwrap_or(&prog).to_string();

        // Device-capable commands are catastrophic regardless of arg
        // shape: the operand may be a bare device path the path
        // classifier would miss inside a flag.
        if DEVICE_CAPABLE.contains(&prog_base.as_str()) {
            findings.push(RiskFinding {
                level: RiskLevel::Catastrophic,
                reason: format!("{prog_base} writes to a raw device"),
                target: prog_base.clone(),
            });
        }

        // Redirections: `>` truncates, `>>` appends. A `>` whose
        // target is a device or a protected path is catastrophic; a
        // `>` anywhere else is at least a Confirm because it destroys
        // the previous contents.
        let mut i = 0;
        while i < args.len() {
            let a = unquote(&args[i]);
            // F2a-11: a truncating redirect may carry an fd prefix
            // (`2>file`, `&>file`) or be the bare `>file`. The
            // pre-fix scan required a leading `>`, so the prefixed
            // forms bypassed the truncating-write check entirely.
            // Strip the prefix, require a single `>` (a `>>` appends
            // and destroys nothing), and reject an fd duplication
            // (`>&2`, which touches no file).
            let redir_target: Option<String> = {
                let rest = if let Some(r) = a.strip_prefix('&') {
                    Some(r)
                } else {
                    let digits = a.find(|c: char| !c.is_ascii_digit()).unwrap_or(a.len());
                    if digits > 0 && a[digits..].starts_with('>') {
                        Some(&a[digits..])
                    } else if a.starts_with('>') {
                        Some(a.as_str())
                    } else {
                        None
                    }
                };
                match rest {
                    Some(r) if r.starts_with('>') && !r.starts_with(">>") => {
                        let inline = &r[1..];
                        if inline.starts_with('&') {
                            // `>&2`: fd duplication, not a file.
                            None
                        } else if inline.is_empty() {
                            // `2>&1` tokenizes as `2>` `&` `1`, so the
                            // target slot holds the fd-dup `&`. That is
                            // not a file; skip it.
                            let next = args.get(i + 1).map(|t| unquote(t));
                            match next {
                                Some(n) if n.starts_with('&') => None,
                                Some(n) => Some(n),
                                None => Some(String::new()),
                            }
                        } else {
                            Some(inline.to_string())
                        }
                    }
                    _ => None,
                }
            };
            if let Some(target) = redir_target {
                if !target.is_empty() && !is_safe_sink(&target) {
                    let (danger, why) = classify_path(&target, ctx);
                    // A truncating write to scratch is the normal
                    // "write a temp file" pattern and destroys
                    // nothing anyone will miss. Anywhere else, `>`
                    // destroys whatever was there, so it is at least
                    // a Confirm — the model may have meant to append
                    // or to edit.
                    let (expanded, _) = crate::paths::expand_lexical(&target, &ctx.home_dir);
                    let in_scratch =
                        crate::paths::normalize(&expanded).starts_with(&ctx.scratch_dir);
                    let level = match danger {
                        PathDanger::Catastrophic => RiskLevel::Catastrophic,
                        _ if in_scratch => RiskLevel::Safe,
                        _ => RiskLevel::Confirm,
                    };
                    if level != RiskLevel::Safe {
                        findings.push(RiskFinding {
                            level,
                            reason: if why.is_empty() {
                                format!("truncating write to {target}")
                            } else {
                                why
                            },
                            target,
                        });
                    }
                }
            }
            i += 1;
        }

        // Path arguments to destructive commands.
        if DESTRUCTIVE.contains(&prog_base.as_str()) {
            let mut saw_path = false;
            for arg in args.iter() {
                let a = unquote(arg);
                if a.starts_with('-') {
                    continue;
                }
                saw_path = true;
                let (danger, why) = classify_path(&a, ctx);
                let level = match danger {
                    PathDanger::Catastrophic => RiskLevel::Catastrophic,
                    PathDanger::Confirm => RiskLevel::Confirm,
                    _ => RiskLevel::Low,
                };
                // A destructive command fed from a pipe cannot be
                // checked: the operands arrive on stdin.
                // T5-C1: xargs forwards its non-flag operands to the
                // wrapped command. Classify them even without a pipe.
                if prog_base == "xargs" {
                    let mut iter = args.iter();
                    while let Some(a) = iter.next() {
                        let a_unq = unquote(a);
                        if a_unq.starts_with('-') { continue; }
                        let wrapped_base = a_unq.rsplit('/').next().unwrap_or(&a_unq).to_string();
                        if DESTRUCTIVE.contains(&wrapped_base.as_str()) {
                            for rest in iter.by_ref() {
                                let rest_unq = unquote(rest);
                                let (danger, why) = classify_path(&rest_unq, ctx);
                                if danger >= PathDanger::Confirm {
                                    findings.push(RiskFinding {
                                        level: RiskLevel::Confirm,
                                        reason: format!("xargs {wrapped_base}: {why}"),
                                        target: rest_unq,
                                    });
                                }
                            }
                        }
                        break;
                    }
                }
                if pipe_fed && prog_base == "xargs" {
                    findings.push(RiskFinding {
                        level: RiskLevel::Confirm,
                        reason: "paths are piped in and cannot be checked statically".to_string(),
                        target: a,
                    });
                } else if level != RiskLevel::Low {
                    findings.push(RiskFinding {
                        level,
                        reason: if why.is_empty() {
                            format!("{prog_base} on {a}")
                        } else {
                            why
                        },
                        target: a,
                    });
                }
            }
            if !saw_path && (prog_base == "rm" || prog_base == "shred") {
                // `rm` with no operand reads from... nothing; but a
                // bare `rm` in a pipe context (`... | xargs rm`) does.
                if pipe_fed {
                    findings.push(RiskFinding {
                        level: RiskLevel::Confirm,
                        reason: format!("{prog_base} with no visible operands in a pipe"),
                        target: prog_base.clone(),
                    });
                }
            }
        }

        // A pipe whose producer is a `find ~` and consumer a
        // destructive command: escalate — the set of affected files
        // is exactly what cannot be checked.
    }

    let level = findings
        .iter()
        .map(|f| f.level)
        .max()
        .unwrap_or(RiskLevel::Safe);

    RiskAssessment { level, findings }
}

fn is_safe_sink(path: &str) -> bool {
    matches!(path, "/dev/null" | "/dev/stdout" | "/dev/stderr")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn ctx() -> RiskContext {
        RiskContext {
            working_dir: PathBuf::from("/work/proj"),
            home_dir: PathBuf::from("/Users/dev"),
            scratch_dir: PathBuf::from("/tmp"),
        }
    }

    #[test]
    fn rm_in_workdir_is_low() {
        let a = assess("rm /work/proj/build.log", &ctx());
        assert!(a.level.runs_immediately(), "got {:?}", a.level);
    }

    #[test]
    fn rm_rf_home_is_catastrophic() {
        let a = assess("rm -rf /Users/dev", &ctx());
        assert!(a.level.is_absolute_deny(), "got {:?}", a.level);
    }

    #[test]
    fn rm_rf_tilde_is_catastrophic() {
        let a = assess("rm -rf ~", &ctx());
        assert!(a.level.is_absolute_deny());
    }

    #[test]
    fn rm_ssh_key_is_catastrophic() {
        let a = assess("rm ~/.ssh/id_rsa", &ctx());
        assert!(a.level.is_absolute_deny());
    }

    #[test]
    fn sudo_is_unwrapped() {
        // `sudo rm` must classify as `rm`, not as an unknown wrapper.
        let a = assess("sudo rm /work/proj/x", &ctx());
        assert!(
            a.level.runs_immediately(),
            "sudo should unwrap: {:?}",
            a.level
        );
    }

    #[test]
    fn env_s_escalates() {
        let a = assess("env -S 'rm -rf /Users/dev'", &ctx());
        assert!(a.level >= RiskLevel::Confirm);
    }

    #[test]
    fn pipe_to_xargs_rm_escalates() {
        // The affected set is decided at runtime.
        let a = assess("find / -type f | xargs rm", &ctx());
        assert!(a.level >= RiskLevel::Confirm, "got {:?}", a.level);
    }

    #[test]
    fn fd_prefixed_redirect_to_a_source_file_confirms() {
        // F2a-11: `2>file` truncates the file just like `>file`; it
        // must not slip past the truncating-write check.
        for cmd in ["cargo build 2> /work/proj/src/main.rs", "cmd &> /work/proj/src/main.rs"] {
            let a = assess(cmd, &ctx());
            assert!(
                a.level >= RiskLevel::Confirm,
                "`{cmd}` must be at least Confirm, got {:?}",
                a.level
            );
        }
    }

    #[test]
    fn append_and_fd_dup_redirects_are_not_truncating() {
        // `2>>` appends, `2>&1` duplicates an fd — neither destroys a
        // file, so neither is a truncating write.
        for cmd in ["cargo build 2>> /work/proj/out.log", "cmd 2>&1"] {
            let a = assess(cmd, &ctx());
            assert!(
                a.level < RiskLevel::Confirm,
                "`{cmd}` is not a truncating file write, got {:?}",
                a.level
            );
        }
    }

    #[test]
    fn redirect_to_dev_null_is_fine() {
        let a = assess("echo hi > /dev/null", &ctx());
        assert!(a.level.runs_immediately(), "got {:?}", a.level);
    }

    #[test]
    fn redirect_over_a_source_file_confirms() {
        let a = assess("echo x > /work/proj/src/main.rs", &ctx());
        assert_eq!(a.level, RiskLevel::Confirm);
    }

    #[test]
    fn heredoc_body_does_not_trip() {
        // The heredoc text contains `rm -rf /` but is data, not a
        // command.
        let cmd = "cat > /tmp/note <<'EOF'\nrm -rf /\nEOF";
        let a = assess(cmd, &ctx());
        assert!(a.level.runs_immediately(), "got {:?}", a.level);
    }

    #[test]
    fn dd_to_device_is_catastrophic() {
        let a = assess("dd if=/dev/zero of=/dev/sda", &ctx());
        assert!(a.level.is_absolute_deny());
    }

    #[test]
    fn variable_in_destructive_path_confirms() {
        let a = assess("rm $TARGET/x", &ctx());
        assert_eq!(a.level, RiskLevel::Confirm);
    }

    #[test]
    fn empty_command_is_safe() {
        assert_eq!(assess("", &ctx()).level, RiskLevel::Safe);
    }

    #[test]
    fn substantive_justification_rejects_affirmations() {
        assert!(!Justification("yes".into()).is_substantive());
        assert!(!Justification("ok".into()).is_substantive());
        assert!(!Justification("short".into()).is_substantive());
        assert!(
            Justification("removing the stale build cache under the project target dir".into())
                .is_substantive()
        );
    }
}

#[cfg(test)]
mod t5_regression {
    use super::*;

    fn ctx() -> RiskContext {
        RiskContext::from_env()
    }

    #[test]
    fn xargs_with_destructive_wrapped_command_is_flagged() {
        let r = assess("xargs rm /etc/passwd", &ctx());
        assert!(
            r.level >= RiskLevel::Confirm,
            "expected Confirm+, got {:?}: {:?}",
            r.level, r.findings,
        );
    }

    #[test]
    fn unterminated_heredoc_does_not_swallow_trailing_command() {
        let r = assess("cat foo <<EOF; rm -rf /tmp/whatever", &ctx());
        assert!(
            r.level >= RiskLevel::Confirm,
            "expected Confirm+, got {:?}: {:?}",
            r.level, r.findings,
        );
    }

    #[test]
    fn terminated_heredoc_does_not_escalate() {
        let r = assess("cat foo <<EOF\nbody\nEOF\n", &ctx());
        // No trailing destructive command — must not fire the sentinel.
        assert!(
            r.findings.iter().all(|f| !f.reason.contains("closing terminator")),
            "terminated heredoc should not escalate: {:?}", r.findings,
        );
    }

    #[test]
    fn benign_command_stays_safe() {
        let r = assess("ls -la", &ctx());
        assert_eq!(r.level, RiskLevel::Safe, "ls flagged: {:?}", r.findings);
    }
}

