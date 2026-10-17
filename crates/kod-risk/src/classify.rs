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
        // T5-C29: reject all-whitespace justifications.
        if self.0.trim().is_empty() {
            return false;
        }

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
/// following argument, and a count of leading positional arguments
/// the wrapper takes before the inner command. A wrapper not in this
/// table stops the unwrap — the classifier cannot see through it,
/// which is a `Confirm`.
///
/// The third field is what `timeout 30 rm -rf /` needs: `30` is a
/// duration, not the binary. Without it, `unwrap` reports
/// `program = "30"`, `30` is not in `DESTRUCTIVE`, and the
/// destructive half runs without risk classification. Same class as
/// the env-assignment bypass fixed just above.

/// SEC-A-04b: `script` / `su` share a shape — optional positional
/// (file / user) before the program, plus `-c CMD` shell form.
/// Returns `(None, [], false)` for the shell form (cannot de-compose
/// the command string); `(Some(prog), args, true)` for positional;
/// `(Some(wrapper), [], true)` for the interactive form (Safe).
fn unwrap_optional_positional_wrapper(
    tokens: &[String],
    wrapper: &str,
) -> (Option<String>, Vec<String>, bool) {
    let dd = tokens.iter().position(|t| unquote(t) == "--");
    let c_end = dd.unwrap_or(tokens.len());

    for idx in 1..c_end {
        if unquote(&tokens[idx]) == "-c" {
            return (None, Vec::new(), false);
        }
    }

    let pos_start = dd.map(|i| i + 1).unwrap_or(1);
    let positionals: Vec<usize> = (pos_start..tokens.len())
        .filter(|&i| {
            let a = unquote(&tokens[i]);
            a != "-" && !a.starts_with('-')
        })
        .collect();

    if positionals.len() >= 2 {
        let prog_idx = positionals[1];
        let prog = strip_quotes(&unquote(&tokens[prog_idx]));
        return (Some(prog), tokens[prog_idx + 1..].to_vec(), true);
    }
    (Some(wrapper.to_string()), tokens[1..].to_vec(), true)
}

const WRAPPERS: &[(&str, &[&str], usize)] = &[
    // Keep in sync with kod-config's PREFIX_WRAPPERS. `sudo -C` (the
    // `--close-from` fd-redirect flag, a number) consumes one token;
    // pre-fix this side omitted it, so `sudo -C 3 rm -rf /` unwrapped
    // to `3` here while kod-config correctly reached `rm`. The
    // cross-crate mirror test in kod-core-tools pins the two tables
    // equal.
    ("sudo", &["-u", "-g", "-p", "-h", "-r", "-t", "-C"], 0),
    ("doas", &["-u"], 0),
    // `env -i` clears the environment and takes NO argument; `-u`,
    // `-C`, `-S` each consume one. The pre-fix list had `-i` among
    // the consuming flags, so `env -i $CMD -rf /` skipped `$CMD` as
    // if it were `-i`'s argument and reported `/Users/dev` as the
    // program — Safe, despite the runtime program being unbounded.
    ("env", &["-u", "-C", "-S"], 0),
    ("nice", &["-n"], 0),
    // `timeout [OPTION] DURATION COMMAND` — one positional (the
    // duration) precedes the inner command.
    ("timeout", &["-s", "-k"], 1),
    ("stdbuf", &["-i", "-o", "-e"], 0),
    ("ionice", &["-c", "-n", "-p"], 0),
    ("time", &[], 0),
    ("command", &[], 0),
    ("builtin", &[], 0),
    ("exec", &["-a", "-c"], 0),
    (
        "xargs",
        &["-n", "-I", "-i", "-P", "-s", "-d", "-a", "-E"],
        0,
    ),
    // SEC-A-04b: wrappers the audit found hidden. Each one is a
    // pass-through that runs the token after it as the actual
    // program. Flags listed are the ones that consume the following
    // token; positional counts reflect the wrapper's own leading
    // non-flag arguments (`timeout 30 rm` shape). Deliberately
    // restricted to wrappers whose flag semantics are well-defined
    // and documented; `flock`, `script`, `chrt`, `su`, `unshare` are
    // omitted because their flag consumption is ambiguous or
    // consumes a command-string (`-c CMD`) that the unwrap loop
    // cannot descend into safely. Those need a follow-up pass.
    ("nohup", &[], 0),
    ("watch", &["-n"], 0),
    ("setsid", &[], 0),
    ("strace", &["-o", "-p", "-s", "-e", "-u"], 0),
    ("ltrace", &["-o", "-p", "-s", "-e", "-u"], 0),
    ("taskset", &["-c", "-p"], 0),
    ("nsenter", &["-t"], 0),
    ("pkexec", &[], 0),
    // SEC-A-04b: three more pass-through wrappers the audit named.
    // `script` and `su` are deliberately absent: both have an OPTIONAL
    // positional (filename / username), so a mechanical entry would
    // misclassify `su rm -rf /` (`rm` is the target user, not the
    // program). Those need per-wrapper custom parsers; the audit's
    // false-positive warning applies.
    //
    // `flock [options] file command` — one positional (the lock file).
    ("flock", &["-e", "-s", "-x", "-n", "-w", "-E", "-o"], 1),
    // `chrt [options] priority command` — one positional (numeric).
    ("chrt", &[], 1),
    // `unshare [options] program` — no positional prefix.
    ("unshare", &[], 0),
];

/// The wrapper table, for the cross-crate mirror test.
///
/// `kod-config::policy::wrapper_tables_for_tests` returns the same
/// shape; the test asserts the two agree element-for-element. The
/// duplication is deliberate (the crates cannot share a dependency),
/// so the regression is the only thing keeping them in sync.
#[doc(hidden)]
pub fn wrapper_table_for_tests() -> &'static [(&'static str, &'static [&'static str], usize)] {
    WRAPPERS
}

/// The command-string wrappers, for the cross-crate mirror test.
///
/// Hoisted to module scope (the pre-fix local `SHELLS` inside
/// `assess` is gone; `assess` now reads this const) so a cross-crate
/// test can compare it against kod-config's list.
pub const SHELLS_FOR_TESTS: &[&str] = &["bash", "sh", "zsh", "dash", "ksh"];

/// Interpreters whose code-string flag runs a program the classifier
/// cannot tokenize. `python3 -c "..."`, `perl -e "..."`, etc. — the
/// same shape as `bash -c`, and escalated for the same reason. The
/// tuple is `(program, code-flag)`.
const INTERPRETERS: &[(&str, &str)] = &[
    ("python", "-c"),
    ("python3", "-c"),
    ("perl", "-e"),
    ("ruby", "-e"),
    ("node", "-e"),
    ("php", "-r"),
    ("lua", "-e"),
];

#[doc(hidden)]
pub fn command_string_wrappers_for_tests() -> &'static [&'static str] {
    SHELLS_FOR_TESTS
}

/// Commands that delete, and whose operand paths must be checked.
const DESTRUCTIVE: &[&str] = &[
    "rm", "rmdir", "mv", "dd", "shred", "truncate", "unlink", "cp",
];

/// Commands that write to a raw device regardless of arg shape — the
/// operand may be a bare device path the path classifier would miss
/// inside a flag, so these are catastrophic outright.
///
/// `tee` also "writes wherever it is pointed", but it is not
/// device-only (a `tee` to scratch is the normal temp-file pattern),
/// so it is classified per-operand alongside the redirect handling
/// rather than listed here.
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
                    heredoc_terminator =
                        Some(term.trim_matches(|c| c == '\'' || c == '"').to_string());
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
                if (c == '&' || c == '|') && i + 1 < chars.len() && chars[i + 1] == c {
                    tokens.push(format!("{c}{c}"));
                    i += 1;
                } else {
                    tokens.push(c.to_string());
                }
            }
            // SEC-A-03: a backslash outside quotes escapes the next
            // character. The shell strips it before word-splitting, so
            // `\rm` runs `rm`. Preserving the next char literally is
            // exactly what the shell does.
            '\\' if !in_single && !in_double && i + 1 < chars.len() => {
                current.push(chars[i + 1]);
                i += 2;
                continue;
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
    // A shell env-prefix: leading `NAME=value` tokens set the
    // environment for the command that follows, they are not the
    // program. `FOO=bar rm -rf /` runs `rm`; pre-fix `unwrap` returned
    // `program = "FOO=bar"` (not in DESTRUCTIVE), so the real program
    // was hidden and the command classified Safe. Same shape as the
    // `env FOO=bar rm` case the wrapper loop already handles.
    while i < tokens.len() && looks_like_env_assignment(&unquote(&tokens[i])) {
        i += 1;
    }
    loop {
        if i >= tokens.len() {
            return (None, Vec::new(), true);
        }
        let tok = unquote(&tokens[i]);
        // SEC-A-03: strip quote characters before matching. The shell
        // removes quotes at word-splitting time, so `'r'm` runs `rm`;
        // the raw token `'r'm` matches neither DESTRUCTIVE nor WRAPPERS.
        let tok = strip_quotes(&tok);
        if tok == "script" || tok == "su" {
            return unwrap_optional_positional_wrapper(&tokens[i..], tok.as_str());
        }
        let Some((_, flag_args, positional_skip)) = WRAPPERS.iter().find(|(w, _, _)| *w == tok)
        else {
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
            } else if looks_like_env_assignment(&a) {
                // `env FOO=bar rm -rf /` — `FOO=bar` is a positional
                // to `env`, not the program. Pre-fix the classifier
                // reported `program = "FOO=bar"` (not in DESTRUCTIVE,
                // not device-capable), so the wrapper bypassed the
                // risk check entirely. A `NAME=value` token can only
                // be a wrapper argument, never an executable name in
                // the tokenizer's world.
                i += 1;
            } else {
                break;
            }
        }
        // Skip leading positional arguments the wrapper takes before
        // the command. `timeout 30 rm -rf /` has `30` as a duration,
        // not the program.
        let mut skipped = 0usize;
        while skipped < *positional_skip && i < tokens.len() {
            let a = unquote(&tokens[i]);
            // A flag here means the parser has drifted; bail out to
            // avoid skipping the command itself.
            if a.starts_with('-') {
                break;
            }
            i += 1;
            skipped += 1;
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

/// True for a token that looks like a `NAME=value` env assignment.
/// Same shape as `kod-config::policy::binary_candidates`'s check —
/// the two parsers keep their tables in sync by hand (kod-config does
/// not depend on kod-risk).
fn looks_like_env_assignment(t: &str) -> bool {
    if t.starts_with('-') || t.starts_with('/') {
        return false;
    }
    let Some((name, _)) = t.split_once('=') else {
        return false;
    };
    !name.is_empty()
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Remove every character from `s` that appears between single quotes
/// (quotes included). Used by the raw-command scan so `echo '$(x)'`
/// (a literal, never executed) is not mistaken for a substitution.
fn strip_single_quoted(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_single = false;
    for c in s.chars() {
        match c {
            '\'' => in_single = !in_single,
            _ if in_single => {}
            _ => out.push(c),
        }
    }
    out
}

/// Strip every quote character from a token, matching the shell's
/// quote removal at word-splitting time. `'r'm` → `rm`.
fn strip_quotes(s: &str) -> String {
    s.chars().filter(|c| *c != '"' && *c != '\'').collect()
}

/// True when `s` contains a lone `&` (a background operator): not `&&`,
/// not `&>`, not `>&`.
fn has_background_ampersand(s: &str) -> bool {
    let b: Vec<char> = s.chars().collect();
    for i in 0..b.len() {
        if b[i] != '&' {
            continue;
        }
        let prev = i.checked_sub(1).map(|j| b[j]);
        let next = b.get(i + 1).copied();
        // `&&`: the second `&` of the pair is skipped by the earlier
        // index; the first is not a background op.
        if next == Some('&') || prev == Some('&') {
            continue;
        }
        // `&>` / `>&`: redirect, not background.
        if next == Some('>') || prev == Some('>') {
            continue;
        }
        return true;
    }
    false
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

    // SEC-A-03/A-04/A-05: the shell runs several constructs that the
    // tokenizer cannot faithfully model — command substitution
    // (`$(...)`, backticks), process substitution (`<(...)`,
    // `>(...)`), a leading subshell `(`, and a lone `&` that starts
    // a new command. Rather than chase each one through tokenization
    // (where `$(` splits into `$` + `(`, and `&>` must not split),
    // scan the raw command once and escalate. The module contract is
    // "never refuses; escalates", so a false positive costs a prompt,
    // not a blocked legitimate command.
    let mut findings: Vec<RiskFinding> = Vec::new();
    {
        let exec_constructs: &[(&str, &str)] = &[
            ("$(", "command substitution"),
            ("`", "backtick substitution"),
            ("<(", "process substitution"),
            (">(", "process substitution"),
        ];
        let unquoted = strip_single_quoted(command);
        for (needle, label) in exec_constructs {
            if unquoted.contains(needle) {
                findings.push(RiskFinding {
                    level: RiskLevel::Confirm,
                    reason: format!(
                        "{label} executes at expansion time and cannot be \
                         inspected statically"
                    ),
                    target: command.to_string(),
                });
                break;
            }
        }
        // A command that begins with a subshell `(` runs in a child
        // shell; the tokenizer glues `(rm` into one token, so the
        // program check sees an unknown program and returns Safe.
        if unquoted.trim_start().starts_with('(') {
            findings.push(RiskFinding {
                level: RiskLevel::Confirm,
                reason: "a subshell `(` starts a command the classifier \
                         cannot inspect statically"
                    .to_string(),
                target: command.to_string(),
            });
        }
        // A lone `&` (not `&>`, not `&&`) starts a new command.
        if has_background_ampersand(&unquoted) {
            findings.push(RiskFinding {
                level: RiskLevel::Confirm,
                reason: "a backgrounded `&` starts a command the classifier \
                         does not inspect in this segment"
                    .to_string(),
                target: command.to_string(),
            });
        }
    }
    let tokens = tokenize(command);

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

        // A shell with a command-string flag is a nested command the
        // classifier cannot see through. `bash script.sh` is fine —
        // no `-c`, the next token is a file. `bash -c 'rm -rf /'`
        // is not: the inner command decides the risk and the
        // classifier has no way to tokenize it here without
        // recursing into `assess`. Escalate rather than return Safe
        // on `bash`. The module doc's contract — "never refuses to
        // run something it does not understand; it escalates" — is
        // exactly this case.
        //
        // The `-c` detection covers the combined-flag form the
        // wrapper parser in `kod-config::policy::binary_candidates`
        // was recently fixed to recognise: `bash -xc 'rm'`, `sh -ec
        // 'rm'`. Missing it here would be a second hole in the same
        // shape.
        // A program name the classifier cannot resolve is an escalation:
        // the classifier does not know what will run, so it cannot
        // conclude `Safe`. The module doc's contract — "never refuses
        // to run something it does not understand; it escalates" — is
        // exactly this case. Four forms:
        //
        // * A variable expansion as the program: `$CMD`, `${CMD}`.
        //   The runtime value is unbounded.
        // * A command substitution as the program: `$(echo rm)`,
        //   `` `echo rm` ``.
        // * `eval`, whose argument is run as a command.
        // * A token that starts with `$` at all in the argument list
        //   is caught by the variable rule below, not here.
        //
        // `env -i $CMD` unwraps to `$CMD` and escalates on the
        // program check; the pre-fix classifier reported the literal
        // `$CMD` string, which was neither in DESTRUCTIVE nor a
        // device — Safe.
        let unresolvable_program = prog_base.contains('$')
            || prog_base.starts_with('`')
            || prog_base.contains('`')
            || prog_base == "eval";
        if unresolvable_program {
            findings.push(RiskFinding {
                level: RiskLevel::Confirm,
                reason: format!("program {prog_base:?} cannot be resolved statically",),
                target: seg.join(" "),
            });
            continue;
        }

        if SHELLS_FOR_TESTS.contains(&prog_base.as_str()) {
            let has_command_flag = args.iter().any(|t| {
                let t = unquote(t);
                if t == "-c" || t == "--command" {
                    return true;
                }
                // Combined short flags: `-xc`, `-cx`, `-ec`. The
                // letter `c` anywhere in the flag cluster is the
                // command-string flag; `=` excludes long-with-value
                // forms already handled.
                t.starts_with('-')
                    && !t.starts_with("--")
                    && t.len() > 1
                    && !t.contains('=')
                    && t[1..].contains('c')
            });
            if has_command_flag {
                findings.push(RiskFinding {
                    level: RiskLevel::Confirm,
                    reason: "shell runs a nested command string the classifier cannot inspect statically"
                        .to_string(),
                    target: seg.join(" "),
                });
                continue;
            }
        }

        // An interpreter with its code-string flag runs a program the
        // classifier cannot see — the same shape as `bash -c`. Pre-fix
        // `python3 -c "..."` / `perl -e "..."` classified Safe while
        // doing arbitrary file mutation.
        if let Some((_, code_flag)) = INTERPRETERS.iter().find(|(p, _)| *p == prog_base) {
            let has_code_flag = args.iter().any(|t| {
                let t = unquote(t);
                t == *code_flag
                    // Combined short flags: `-ce`, `-ec`.
                    || (t.starts_with('-')
                        && !t.starts_with("--")
                        && t.len() > 1
                        && t[1..].contains(code_flag.trim_start_matches('-')))
            });
            if has_code_flag {
                findings.push(RiskFinding {
                    level: RiskLevel::Confirm,
                    reason: format!(
                        "{prog_base} runs a code string the classifier cannot inspect statically"
                    ),
                    target: seg.join(" "),
                });
                continue;
            }
        }

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
                // `classify_path` expands `~`, `$HOME`, and `${HOME}`
                // lexically. What it cannot resolve — a command
                // substitution (`$(...)`, backticks) or a variable it
                // does not know — must not fall through as Safe: the
                // module doc's contract is "never refuses to run
                // something it does not understand; it escalates".
                // Pre-fix `rm -rf $(echo ~)` classified Safe, defeating
                // the unoverridable Catastrophic gate a literal
                // `rm -rf ~` hits. Only escalate when `classify_path`
                // found nothing, so a resolvable `$HOME` keeps its
                // Catastrophic level.
                let unresolved_operand = (a.contains('$') || a.contains('`'))
                    && matches!(danger, PathDanger::Safe | PathDanger::Low);
                if unresolved_operand {
                    findings.push(RiskFinding {
                        level: RiskLevel::Confirm,
                        reason: format!("{prog_base} operand {a:?} cannot be resolved statically"),
                        target: a.clone(),
                    });
                    continue;
                }
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
                        if a_unq.starts_with('-') {
                            continue;
                        }
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
                // SEC-A-18: a stdin redirect (`xargs rm -rf < targets`)
                // delivers operands the same way the pipe does — the
                // pre-fix check only fired on `pipe_fed`, so the
                // redirect variant classified Safe. Both shapes route
                // unclassified arguments to `rm`.
                let stdin_redirect = seg
                    .iter()
                    .map(|t| unquote(t))
                    .any(|t| t == "<" || t == "<<" || t.starts_with("<<<"));
                if pipe_fed || stdin_redirect {
                    findings.push(RiskFinding {
                        level: RiskLevel::Confirm,
                        reason: format!(
                            "{prog_base} with no visible operands in a pipe or stdin redirect"
                        ),
                        target: prog_base.clone(),
                    });
                }
            }
        }

        // `tee` writes wherever it is pointed — the DEVICE_CAPABLE
        // doc names it, but it is not a device-only command (a `tee`
        // to scratch is the normal temp-file pattern), so it is not in
        // that list. Instead classify each non-flag operand the way a
        // redirect target is classified: a credential store or system
        // path is Catastrophic, scratch is Safe, anything else is at
        // least Confirm (the write destroys prior contents).
        // SEC-A-05: `$(...)`, backticks, and `<(...)`/`>(...)`
        // execute at expansion time regardless of the wrapping program.
        // `echo $(rm -rf ~)` runs `rm` first. Escalate any argument
        // carrying one; the classifier cannot see inside.
        if args
            .iter()
            .map(|t| unquote(t))
            .any(|a| a.contains("$(") || a.contains('`') || a.contains("<(") || a.contains(">("))
        {
            findings.push(RiskFinding {
                level: RiskLevel::Confirm,
                reason: "argument contains a shell expansion that executes \
                         at expansion time and cannot be inspected statically"
                    .to_string(),
                target: seg.join(" "),
            });
            continue;
        }

        if prog_base == "tee" {
            for arg in args.iter() {
                let a = unquote(arg);
                if a.starts_with('-') {
                    continue;
                }
                if is_safe_sink(&a) {
                    continue;
                }
                let (danger, why) = classify_path(&a, ctx);
                let (expanded, _) = crate::paths::expand_lexical(&a, &ctx.home_dir);
                let in_scratch = crate::paths::normalize(&expanded).starts_with(&ctx.scratch_dir);
                let level = match danger {
                    PathDanger::Catastrophic => RiskLevel::Catastrophic,
                    _ if in_scratch => RiskLevel::Safe,
                    _ => RiskLevel::Confirm,
                };
                if level != RiskLevel::Safe {
                    findings.push(RiskFinding {
                        level,
                        reason: if why.is_empty() {
                            format!("tee writes to {a}")
                        } else {
                            why
                        },
                        target: a,
                    });
                }
            }
        }

        // In-place writers: `sed -i`, `sort -o FILE`, and `install`
        // overwrite a named file — the same blast radius as `cp` /
        // `truncate` (both in DESTRUCTIVE), but they were not
        // classified, so a write to a credential path came back Safe.
        // Extract the file operand(s) each one writes and classify them.
        let in_place_targets: Vec<String> = match prog_base.as_str() {
            // `sed -i` writes each file operand; the script (`s/x/y/`)
            // is the first positional and is not a path. Only the -i
            // form writes.
            "sed"
                if args.iter().any(|t| {
                    let t = unquote(t);
                    t == "-i"
                        || t.starts_with("-i")
                        || t == "--in-place"
                        || t.starts_with("--in-place")
                }) =>
            {
                let rest = args.iter().map(|t| unquote(t));
                let mut script_seen = false;
                let mut out = Vec::new();
                for a in rest {
                    if a.starts_with('-') {
                        // `-i` may take an optional suffix argument
                        // (`-i.bak` carries it inline; a bare `-i`
                        // followed by `''` consumes nothing here
                        // because the script follows).
                        continue;
                    }
                    if !script_seen {
                        script_seen = true;
                        continue;
                    }
                    out.push(a);
                }
                out
            }
            // `sort -o FILE`: the -o flag's value is the output.
            "sort" => {
                let mut out = Vec::new();
                let mut iter = args.iter().map(|t| unquote(t));
                while let Some(a) = iter.next() {
                    if a == "-o" || a == "--output" {
                        if let Some(v) = iter.next() {
                            out.push(v);
                        }
                    } else if let Some(v) = a.strip_prefix("--output=") {
                        out.push(v.to_string());
                    } else if let Some(v) = a.strip_prefix("-o") {
                        // `-oFILE` combined form.
                        if !v.is_empty() {
                            out.push(v.to_string());
                        }
                    }
                }
                out
            }
            // `install`: the last non-flag operand is the destination.
            "install" => {
                let positional: Vec<String> = args
                    .iter()
                    .map(|t| unquote(t))
                    .filter(|t| !t.starts_with('-'))
                    .collect();
                positional.last().cloned().into_iter().collect()
            }
            // `ln TARGET LINK` (optionally -s/-f): the LAST positional
            // is the link name, which is created or replaced. On a
            // credential path that destroys the file's contents. Only
            // the link operand is classified, not the target.
            // SEC-A-09: classify ALL positionals, not just the link.
            // `ln ~/.ssh/id_rsa ./notes.txt` (a hardlink) makes the key
            // readable at a workdir path.
            "ln" => args
                .iter()
                .map(|t| unquote(t))
                .filter(|t| !t.starts_with('-'))
                .collect(),
            _ => Vec::new(),
        };
        for target in &in_place_targets {
            if is_safe_sink(target) {
                continue;
            }
            let (danger, why) = classify_path(target, ctx);
            let level = match danger {
                PathDanger::Catastrophic => RiskLevel::Catastrophic,
                PathDanger::Confirm => RiskLevel::Confirm,
                _ => RiskLevel::Low,
            };
            if level != RiskLevel::Low {
                findings.push(RiskFinding {
                    level,
                    reason: if why.is_empty() {
                        format!("{prog_base} writes {target}")
                    } else {
                        why
                    },
                    target: target.clone(),
                });
            }
        }

        // `find` with `-delete` or `-exec` mutates everything it
        // walks. The walk root decides the blast radius, so classify
        // it the same way a destructive command's operand is: a root
        // under home (or a system path) is Catastrophic. A read-only
        // `find` stays Safe. Pre-fix `find ~ -delete` classified Safe:
        // `find` is not in DESTRUCTIVE, and only the `find | xargs`
        // pipe form was caught.
        if prog_base == "find" {
            let mutates = args.iter().any(|t| {
                let t = unquote(t);
                t == "-delete" || t == "-exec" || t == "-execdir" || t == "-ok"
            });
            if mutates {
                // The first non-flag operand is the walk root; find
                // defaults to `.` (the working dir, safe) when absent.
                let root = args
                    .iter()
                    .map(|t| unquote(t))
                    .find(|t| !t.starts_with('-'));
                match root {
                    Some(r) => {
                        let (danger, why) = classify_path(&r, ctx);
                        let level = match danger {
                            PathDanger::Catastrophic => RiskLevel::Catastrophic,
                            _ => RiskLevel::Confirm,
                        };
                        findings.push(RiskFinding {
                            level,
                            reason: if why.is_empty() {
                                format!("find {r} mutates everything it walks")
                            } else {
                                why
                            },
                            target: r,
                        });
                    }
                    None => {
                        // No explicit root: `.` — the working dir. A
                        // mutating find there still destroys files the
                        // user may not expect, so Confirm.
                        findings.push(RiskFinding {
                            level: RiskLevel::Confirm,
                            reason: "find with -delete/-exec over the working directory"
                                .to_string(),
                            target: ".".to_string(),
                        });
                    }
                }
            }
        }
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
        for cmd in [
            "cargo build 2> /work/proj/src/main.rs",
            "cmd &> /work/proj/src/main.rs",
        ] {
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
    fn destructive_with_a_substituted_path_is_not_safe() {
        // The module doc's contract: "never refuses to run something
        // it does not understand; it escalates". A command substitution
        // as the *program* already escalates; as the *argument* to a
        // destructive command it did not, so `rm -rf $(echo ~)`
        // classified Safe — defeating the unoverridable Catastrophic
        // gate that a literal `rm -rf ~` hits.
        for c in [
            "rm -rf $(echo /Users/dev)",
            "rm -rf `echo /Users/dev`",
            "rm -rf ${TARGET}",
            "rm -rf $HOME",
        ] {
            let a = assess(c, &ctx());
            assert!(
                a.level != RiskLevel::Safe,
                "{c:?} must not be Safe (got {:?})",
                a.level,
            );
        }
    }

    #[test]
    fn in_place_writer_to_credential_store_is_catastrophic() {
        // `sed -i`, `sort -o`, and `install` overwrite a named file —
        // the same blast radius as `cp`/`truncate` (both in
        // DESTRUCTIVE), but they were not classified, so a write to a
        // credential path came back Safe.
        for c in [
            "sed -i 's/x/y/' /Users/dev/.ssh/authorized_keys",
            "sort -o /Users/dev/.ssh/authorized_keys /dev/null",
            "install -m 600 /tmp/x /Users/dev/.ssh/authorized_keys",
        ] {
            let a = assess(c, &ctx());
            assert!(a.level.is_absolute_deny(), "{c:?} got {:?}", a.level);
        }
    }

    #[test]
    fn in_place_writer_in_workdir_is_not_catastrophic() {
        let a = assess("sed -i 's/x/y/' /work/proj/src/a.rs", &ctx());
        assert!(!a.level.is_absolute_deny(), "got {:?}", a.level);
    }

    #[test]
    fn ln_into_credential_store_is_catastrophic() {
        // `ln -sf TARGET LINK` replaces LINK with a symlink. On a
        // credential path that destroys the file's contents (the link
        // points elsewhere) — the same blast radius as the `cp`/
        // `truncate`/`sed -i` writers already classified, but `ln` is
        // unclassified, so it came back Safe.
        for c in [
            "ln -sf /dev/null /Users/dev/.ssh/authorized_keys",
            "ln -sfn /tmp/x /Users/dev/.ssh/id_rsa",
            "ln -s /tmp/x /etc/sudoers",
        ] {
            let a = assess(c, &ctx());
            assert!(a.level.is_absolute_deny(), "{c:?} got {:?}", a.level);
        }
    }

    #[test]
    fn ln_in_workdir_is_not_catastrophic() {
        let a = assess("ln -s target /work/proj/link", &ctx());
        assert!(!a.level.is_absolute_deny(), "got {:?}", a.level);
    }

    #[test]
    fn leading_env_assignment_does_not_hide_the_program() {
        // `FOO=bar rm -rf /Users/dev` runs `rm` with `FOO=bar` in its
        // environment (a standard shell env-prefix). `unwrap` returned
        // `program = "FOO=bar"` — not in DESTRUCTIVE — because the
        // env-assignment skip ran only *inside* a wrapper, so a bare
        // leading assignment hid the real program and the command
        // classified Safe. Same shape as the fixed `env FOO=bar rm`.
        let a = assess("FOO=bar rm -rf /Users/dev", &ctx());
        assert!(a.level.is_absolute_deny(), "got {:?}", a.level);
    }

    #[test]
    fn multiple_leading_env_assignments_unwrap() {
        let a = assess("A=1 B=2 shred /Users/dev/.ssh/id_rsa", &ctx());
        assert!(a.level.is_absolute_deny(), "got {:?}", a.level);
    }

    #[test]
    fn a_leading_assignment_then_a_safe_command_stays_safe() {
        let a = assess("FOO=bar ls /work/proj", &ctx());
        assert!(a.level.runs_immediately(), "got {:?}", a.level);
    }

    #[test]
    fn backslash_escaped_program_does_not_hide_rm() {
        for c in [
            "\\rm -rf /Users/dev",
            "r\\m -rf /Users/dev",
            "'r'm -rf /Users/dev",
        ] {
            let a = assess(c, &ctx());
            assert!(a.level != RiskLevel::Safe, "{c:?} got {:?}", a.level);
        }
    }

    #[test]
    fn subshell_program_does_not_hide_rm() {
        // The raw scan escalates a subshell to Confirm (the module
        // contract: "escalates, never Safe") rather than running the
        // inner command unclassified.
        let a = assess("(rm -rf /Users/dev)", &ctx());
        assert!(a.level != RiskLevel::Safe, "got {:?}", a.level);
    }

    #[test]
    fn lone_amp_splits_and_classifies_the_second_command() {
        // A lone `&` is escalated to Confirm (the second command is not
        // classified in this segment), never Safe.
        let a = assess("sleep 1 & rm -rf /Users/dev", &ctx());
        assert!(a.level != RiskLevel::Safe, "got {:?}", a.level);
    }

    #[test]
    fn substitution_as_an_argument_escalates() {
        for c in [
            "echo $(rm -rf /Users/dev)",
            "ls `rm -rf /Users/dev`",
            "cat <(rm -rf /Users/dev)",
        ] {
            let a = assess(c, &ctx());
            assert!(a.level != RiskLevel::Safe, "{c:?} got {:?}", a.level);
        }
    }

    #[test]
    fn tilde_user_form_escalates() {
        for c in ["rm -rf ~dev", "rm -rf ~+", "rm -rf ~root/.ssh"] {
            let a = assess(c, &ctx());
            assert!(a.level != RiskLevel::Safe, "{c:?} got {:?}", a.level);
        }
    }

    #[test]
    fn working_dir_itself_is_not_safe() {
        let a = assess("rm -rf /work/proj", &ctx());
        assert!(!a.level.runs_immediately(), "got {:?}", a.level);
    }

    #[test]
    fn ln_target_is_classified() {
        // `ln` with a hardlink target under a credential store: the
        // target is a read operand the blast radius depends on.
        let a = assess("ln /Users/dev/.ssh/id_rsa ./notes.txt", &ctx());
        assert!(a.level.is_absolute_deny(), "got {:?}", a.level);
    }

    #[test]
    fn interpreter_code_flag_escalates() {
        // An interpreter running a code string the classifier cannot
        // see is the same shape as `bash -c`: the inner operation is
        // unbounded. The shell case escalates; these did not, so
        // `python3 -c "open(...,'w')"` and `perl -e "unlink ..."`
        // classified Safe while doing arbitrary file mutation.
        for c in [
            "python3 -c \"open('/Users/dev/.ssh/id_rsa','w').close()\"",
            "python -c \"import os; os.remove('/Users/dev/.bashrc')\"",
            "perl -e \"unlink '/Users/dev/.ssh/id_rsa'\"",
            "ruby -e \"File.delete('/Users/dev/.bashrc')\"",
            "node -e \"require('fs').unlinkSync('/Users/dev/.bashrc')\"",
        ] {
            let a = assess(c, &ctx());
            assert!(
                a.level != RiskLevel::Safe,
                "{c:?} must not be Safe (got {:?})",
                a.level,
            );
        }
    }

    #[test]
    fn interpreter_script_file_is_not_escalated() {
        // `python3 script.py` runs a file the classifier can read, so
        // it is not the opaque code-string case.
        let a = assess("python3 /work/proj/script.py", &ctx());
        assert!(a.level.runs_immediately(), "got {:?}", a.level);
    }

    #[test]
    fn find_delete_over_home_is_catastrophic() {
        // `find ~ -delete` deletes everything under home; `find` is not
        // in DESTRUCTIVE, and only the `find | xargs` pipe form was
        // caught, so the -delete/-exec forms classified Safe.
        let a = assess("find /Users/dev -type f -delete", &ctx());
        assert!(a.level.is_absolute_deny(), "got {:?}", a.level);
    }

    #[test]
    fn find_exec_rm_over_home_is_catastrophic() {
        let a = assess("find /Users/dev -exec rm {} +", &ctx());
        assert!(a.level.is_absolute_deny(), "got {:?}", a.level);
    }

    #[test]
    fn find_delete_in_workdir_is_not_catastrophic() {
        let a = assess("find /work/proj -name '*.tmp' -delete", &ctx());
        assert!(!a.level.is_absolute_deny(), "got {:?}", a.level);
    }

    #[test]
    fn plain_find_over_home_is_not_catastrophic() {
        // A read-only find is harmless.
        let a = assess("find /Users/dev -type f", &ctx());
        assert!(a.level.runs_immediately(), "got {:?}", a.level);
    }

    #[test]
    fn tee_to_a_credential_store_is_catastrophic() {
        // `tee` writes wherever it is pointed — the DEVICE_CAPABLE
        // doc names it as covered, but the list omitted it, so
        // `tee ~/.ssh/authorized_keys` classified Safe and wrote
        // arbitrary content to a credential path.
        let a = assess("tee /Users/dev/.ssh/authorized_keys", &ctx());
        assert!(a.level.is_absolute_deny(), "got {:?}", a.level);
    }

    #[test]
    fn tee_to_a_system_file_is_catastrophic() {
        let a = assess("echo x | tee /etc/sudoers", &ctx());
        assert!(a.level.is_absolute_deny(), "got {:?}", a.level);
    }

    #[test]
    fn tee_to_scratch_stays_low() {
        // The normal "write a temp file" pattern must not escalate.
        let a = assess("echo x | tee /tmp/build.log", &ctx());
        assert!(a.level.runs_immediately(), "got {:?}", a.level);
    }

    #[test]
    fn tee_to_a_workdir_file_is_at_most_confirm() {
        let a = assess("echo x | tee /work/proj/out.txt", &ctx());
        assert!(!a.level.is_absolute_deny(), "got {:?}", a.level);
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

    /// A shell running a nested command string must never be `Safe`.
    /// The classifier cannot tokenize `'rm -rf /'` (a single quoted
    /// argument from `bash`'s perspective), so the design's rule —
    /// "an unrecognised wrapper escalates" — applies. Pre-fix the
    /// classifier reported `program = bash`, `bash` was not in
    /// `DESTRUCTIVE`, and the assessment was `Safe`: a
    /// `bash -c 'rm -rf /'` request passed the gate.
    #[test]
    fn bash_c_wrapping_a_destructive_command_escalates() {
        for cmd in [
            "bash -c 'rm -rf /'",
            "sh -c 'rm -rf /'",
            "zsh -c 'rm -rf /'",
            "bash --command 'rm -rf /'",
        ] {
            let a = assess(cmd, &ctx());
            assert!(
                a.level >= RiskLevel::Confirm,
                "{cmd:?} must at least Confirm; got {:?}",
                a.level,
            );
        }
    }

    /// The combined-flag form: `bash -xc 'rm -rf /'` — the flag
    /// cluster contains `c`. Same reasoning as
    /// `binary_candidates`'s fix in kod-config.
    #[test]
    fn bash_combined_short_flags_escalate() {
        for cmd in [
            "bash -xc 'rm -rf /'",
            "bash -cx 'rm -rf /'",
            "sh -ec 'rm -rf /'",
            "bash -euxc 'rm -rf /'",
        ] {
            let a = assess(cmd, &ctx());
            assert!(
                a.level >= RiskLevel::Confirm,
                "{cmd:?} must at least Confirm; got {:?}",
                a.level,
            );
        }
    }

    /// A shell *without* a command-string flag is not escalated: the
    /// next token is a script file, and the script itself is not
    /// analysed (out of scope for this classifier). A benign
    /// `bash script.sh` must stay `Safe` — the shell wrapper itself
    /// is not risky.
    #[test]
    fn a_shell_running_a_script_is_not_escalated_by_the_shell_rule() {
        let a = assess("bash script.sh", &ctx());
        assert!(
            a.level < RiskLevel::Confirm,
            "a script-file shell invocation should not escalate by itself: {:?}",
            a.level,
        );
    }

    /// `sudo bash -c 'rm -rf /'` — the outer wrapper is `sudo`, which
    /// unwrap peels back to `bash`. The shell escalation must fire on
    /// the unwrapped program too, not just on a leading token.
    #[test]
    fn sudo_bash_c_escalates_after_unwrap() {
        let a = assess("sudo bash -c 'rm -rf /'", &ctx());
        assert!(
            a.level >= RiskLevel::Confirm,
            "sudo bash -c must Confirm; got {:?}",
            a.level,
        );
    }

    /// `env FOO=bar rm -rf /` — `FOO=bar` is a positional argument
    /// to `env`, not the program. Pre-fix `unwrap` reported
    /// `program = "FOO=bar"`, which is neither in `DESTRUCTIVE` nor
    /// device-capable, so the destructive half ran without any risk
    /// classification. Same class as the `bash -c` bypass the
    /// previous commit fixed: the wrapper parser must walk past
    /// tokens that are *not* the program.
    #[test]
    fn env_assignment_is_skipped_not_treated_as_a_program() {
        for cmd in [
            "env FOO=bar rm -rf /Users/dev",
            "env A=1 B=2 rm -rf /Users/dev",
            "env LC_ALL=C rm -rf /Users/dev",
        ] {
            let a = assess(cmd, &ctx());
            assert!(
                a.level.is_absolute_deny(),
                "{cmd:?} must deny; got {:?}",
                a.level,
            );
        }
    }

    /// `env FOO=bar bash -c 'rm -rf /'` — same skip, then the shell
    /// escalation fires on the unwrapped `bash`.
    #[test]
    fn env_assignment_then_bash_c_escalates() {
        let a = assess("env FOO=bar bash -c 'rm -rf /'", &ctx());
        assert!(
            a.level >= RiskLevel::Confirm,
            "env FOO=bar bash -c must Confirm; got {:?}",
            a.level,
        );
    }

    /// `timeout 30 rm -rf /Users/dev` — `30` is a duration, not the
    /// binary. Pre-fix `unwrap` reported `program = "30"`, which is
    /// neither in `DESTRUCTIVE` nor device-capable, so the risk
    /// check was bypassed. Same class as the env-assignment bypass
    /// just above.
    #[test]
    fn timeout_duration_positional_is_skipped() {
        for cmd in [
            "timeout 30 rm -rf /Users/dev",
            "timeout 5 rm -rf /Users/dev",
            "timeout -s KILL 30 rm -rf /Users/dev",
            "timeout -k 5 30 rm -rf /Users/dev",
        ] {
            let a = assess(cmd, &ctx());
            assert!(
                a.level.is_absolute_deny(),
                "{cmd:?} must deny; got {:?}",
                a.level,
            );
        }
    }

    /// A program name the classifier cannot resolve — a variable
    /// expansion, a command substitution, `eval` — must escalate.
    /// Pre-fix the classifier reported `program = "$CMD"`, which is
    /// neither in `DESTRUCTIVE` nor device-capable, so the assessment
    /// was `Safe` and the runtime value ran unclassified.
    #[test]
    fn variable_as_program_escalates() {
        for cmd in [
            "$CMD -rf /Users/dev",
            "${CMD} -rf /Users/dev",
            "eval 'rm -rf /Users/dev'",
            "$(echo rm) -rf /Users/dev",
            "`echo rm` -rf /Users/dev",
            "env -i $CMD -rf /Users/dev",
        ] {
            let a = assess(cmd, &ctx());
            assert!(
                a.level >= RiskLevel::Confirm,
                "{cmd:?} must at least Confirm; got {:?}",
                a.level,
            );
        }
    }
}

#[cfg(test)]
mod t5_regression {
    use super::*;

    fn ctx() -> RiskContext {
        RiskContext::from_env()
    }

    #[cfg(unix)]
    #[test]
    fn xargs_with_destructive_wrapped_command_is_flagged() {
        let r = assess("xargs rm /etc/passwd", &ctx());
        assert!(
            r.level >= RiskLevel::Confirm,
            "expected Confirm+, got {:?}: {:?}",
            r.level,
            r.findings,
        );
    }

    #[test]
    fn unterminated_heredoc_does_not_swallow_trailing_command() {
        let r = assess("cat foo <<EOF; rm -rf /tmp/whatever", &ctx());
        assert!(
            r.level >= RiskLevel::Confirm,
            "expected Confirm+, got {:?}: {:?}",
            r.level,
            r.findings,
        );
    }

    #[test]
    fn terminated_heredoc_does_not_escalate() {
        let r = assess("cat foo <<EOF\nbody\nEOF\n", &ctx());
        // No trailing destructive command — must not fire the sentinel.
        assert!(
            r.findings
                .iter()
                .all(|f| !f.reason.contains("closing terminator")),
            "terminated heredoc should not escalate: {:?}",
            r.findings,
        );
    }

    #[test]
    fn benign_command_stays_safe() {
        let r = assess("ls -la", &ctx());
        assert_eq!(r.level, RiskLevel::Safe, "ls flagged: {:?}", r.findings);
    }

    #[test]
    fn whitespace_only_justification_is_not_substantive() {
        // T5-C29: a 25-space justification used to pass the length
        // floor. It must be rejected.
        let padded = " ".repeat(40);
        assert!(!Justification(padded).is_substantive());
        assert!(!Justification("\t\n   \t".to_string()).is_substantive());
        assert!(!Justification(String::new()).is_substantive());
    }
    #[test]
    fn flock_chrt_unshare_are_unwrapped() {
        // SEC-A-04b: three wrappers a naive classifier treats as the
        // program. Each takes a positional prefix before the command,
        // so `prog_base` must be the INNER program (`rm`), not the
        // wrapper. Target is `~` — always expands to the fixture's
        // home_dir, which `classify` treats as Catastrophic. `/Users/
        // dev` only qualifies when the fixture's home is that exact
        // path (it is not); the audit's own `rm -rf ~` test uses `~`
        // for the same reason.
        let ctx = ctx();
        for cmd in [
            "flock /tmp/lock rm -rf ~",
            "chrt 50 rm -rf ~",
            "unshare rm -rf ~",
        ] {
            let a = assess(cmd, &ctx);
            assert!(
                matches!(a.level, RiskLevel::Catastrophic),
                "wrapper not unwrapped for {cmd:?}: got {:?}",
                a.level,
            );
        }
    }
    #[test]
    fn script_and_su_hide_their_inner_command() {
        let ctx = ctx();
        for cmd in ["script -c 'rm -rf /'", "su -c 'rm -rf /'"] {
            let a = assess(cmd, &ctx);
            assert!(
                !matches!(a.level, RiskLevel::Safe),
                "cmd-string form not escalated for {cmd:?}: {:?}",
                a.level,
            );
        }
        for cmd in [
            "script /tmp/log rm -rf ~",
            "su root rm -rf ~",
            "su - root rm -rf ~",
        ] {
            let a = assess(cmd, &ctx);
            assert!(
                matches!(a.level, RiskLevel::Catastrophic),
                "positional form not unwrapped for {cmd:?}: {:?}",
                a.level,
            );
        }
        for cmd in ["script /tmp/log", "su root", "su"] {
            let a = assess(cmd, &ctx);
            assert!(
                matches!(a.level, RiskLevel::Safe),
                "false positive for {cmd:?}: {:?}",
                a.level,
            );
        }
    }
}
