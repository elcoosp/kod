//! The minimizer pipeline: a def is a list of stages; a stage is a
//! small transform.
//!
//! # Shape
//!
//! A [`Def`] deserializes from TOML (embedded at compile time via
//! `include_str!`). Each stage is a tagged enum with a `kind` field
//! and stage-specific parameters. Running a def is a fold over the
//! stages: `text -> stage1 -> stage2 -> ... -> final`.
//!
//! # What this does NOT do
//!
//! * Not a script. Stages are pure functions of the text and their
//!   own parameters; no stage reads a file, calls a subprocess, or
//!   keeps state across runs.
//! * Not a filter chain that can drop a stage's error. A stage that
//!   fails (a bad regex) aborts the pipeline with a named error and
//!   the caller falls back to the raw capture. Silent partial
//!   rewrites are worse than a clean refusal.

use serde::Deserialize;

/// One minimizer definition.
#[derive(Debug, Clone, Deserialize)]
pub struct Def {
    /// Always `1` for the initial format. A def whose
    /// `schema_version` is not `1` fails to parse, which lets a
    /// future format change be a hard break rather than a silent
    /// mis-read.
    pub schema_version: u32,
    /// The def's identifier. Matches the file name (without `.toml`)
    /// by convention; the minimizer does not enforce it.
    pub id: String,
    /// The program this def applies to (`git`, `cargo`, `pytest`).
    pub program: String,
    /// Subcommands the def applies to. Empty means "any subcommand
    /// for this program". For `git`, `["status"]` scopes to
    /// `git status`. Matching is on the *first* argument, which is
    /// the subcommand in every CLI this def ships for.
    #[serde(default)]
    pub subcommands: Vec<String>,
    /// Exit code the def requires. `None` means the def applies
    /// regardless of exit code; `Some(0)` means only on success.
    /// Most defs set this — a `git status` that failed is showing
    /// an error message, not a status.
    #[serde(default)]
    pub only_on_exit: Option<i32>,
    /// The stages to run.
    pub stages: Vec<Stage>,
}

impl Def {
    /// Parse a def from TOML.
    pub fn from_toml(text: &str) -> Result<Self, DefError> {
        let raw: DefRaw = toml::from_str(text).map_err(|e| DefError::Parse {
            reason: e.to_string(),
        })?;
        if raw.schema_version != 1 {
            return Err(DefError::Parse {
                reason: format!(
                    "unsupported schema_version {}; expected 1",
                    raw.schema_version,
                ),
            });
        }
        Ok(Def {
            schema_version: raw.schema_version,
            id: raw.id,
            program: raw.program,
            subcommands: raw.subcommands,
            only_on_exit: raw.only_on_exit,
            stages: raw.stages,
        })
    }

    /// Whether this def applies to a `(program, args)` pair.
    pub fn matches(&self, program: &str, args: &[String]) -> bool {
        if program != self.program {
            return false;
        }
        if self.subcommands.is_empty() {
            return true;
        }
        let Some(first) = args.first() else {
            return false;
        };
        self.subcommands.iter().any(|s| s == first)
    }
}

/// The TOML shape before validation. Splitting it from [`Def`] means
/// the parse error is on the *TOML* and the schema check is on the
/// *semantic* — a caller reads a `Parse` variant with a clear message
/// either way.
#[derive(Debug, Clone, Deserialize)]
struct DefRaw {
    schema_version: u32,
    id: String,
    program: String,
    #[serde(default)]
    subcommands: Vec<String>,
    #[serde(default)]
    only_on_exit: Option<i32>,
    stages: Vec<Stage>,
}

/// A single pipeline stage.
///
/// Deserializes from a `{ kind = "...", ... }` table. The variant set
/// is deliberately small — each stage earns its place by being needed
/// by a shipped def.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Stage {
    /// Remove ANSI escape sequences (`\x1b[31m`, `\x1b[0m`, …).
    /// Applied first in every def; the model never needs the colour
    /// bytes.
    StripAnsi,
    /// Replace every occurrence of `pattern` (a regex) with
    /// `replacement`. The `replacement` may reference capture groups
    /// with `$1`-style syntax.
    Replace {
        pattern: String,
        replacement: String,
    },
    /// Keep only lines matching at least one of `patterns`. An
    /// inverted variant is `strip_lines`.
    KeepLines { patterns: Vec<String> },
    /// Drop every line matching any of `patterns`.
    StripLines { patterns: Vec<String> },
    /// Keep the first `n` lines.
    HeadLines { n: usize },
    /// Keep the last `n` lines.
    TailLines { n: usize },
    /// Hard cap on total lines. Equivalent to `head_lines` when the
    /// intent is "the output was too long"; kept separate so a def
    /// can express which bound it means without the reader guessing.
    MaxLines { n: usize },
}

/// A pipeline stage failed.
#[derive(Debug, Clone)]
pub enum PipelineError {
    /// A stage could not be run. `stage` is the kind as a string
    /// (`"replace"`, `"keep_lines"`); `reason` names the specific
    /// failure (a bad regex, an empty pattern list).
    Stage {
        stage: &'static str,
        reason: String,
    },
}

impl std::fmt::Display for PipelineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stage { stage, reason } => write!(f, "{stage}: {reason}"),
        }
    }
}

impl std::error::Error for PipelineError {}

/// A def-level failure, from parsing.
#[derive(Debug, Clone)]
pub enum DefError {
    Parse { reason: String },
}

impl std::fmt::Display for DefError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parse { reason } => write!(f, "parse: {reason}"),
        }
    }
}

impl std::error::Error for DefError {}

/// Run a stage list over `raw`.
///
/// Every stage is a pure `String -> String`. The function is
/// `pub(crate)` — callers go through [`crate::Minimizer::minimize`].
pub(crate) fn run(stages: &[Stage], raw: &str) -> Result<String, PipelineError> {
    let mut text = raw.to_string();
    for stage in stages {
        text = run_one(stage, text)?;
    }

    // Safety valve. A pipeline that reduced non-empty input to
    // empty output is almost certainly a filter that did not match
    // the shape it expected: a `git log` def applied to
    // `git log --oneline` output finds no `commit ` lines and
    // would otherwise hand the model an empty string. Returning the
    // raw input is strictly better than returning nothing — the
    // model still has the output to read, and the caller's
    // `Minimized` shape records that a def *did* run (the filter
    // field is set), so a caller that wants to know "was this a
    // pass-through?" checks the byte delta, not the filter alone.
    //
    // The valve is a deliberate trade: a def cannot produce an
    // empty result on purpose. No shipped def wants that, and an
    // author who did would be writing a stage that drops
    // everything, which is a mistake worth catching here rather
    // than in a downstream user's confusion.
    if text.trim().is_empty() && !raw.trim().is_empty() {
        return Ok(raw.to_string());
    }
    Ok(text)
}

fn run_one(stage: &Stage, input: String) -> Result<String, PipelineError> {
    match stage {
        Stage::StripAnsi => Ok(strip_ansi(&input)),
        Stage::Replace {
            pattern,
            replacement,
        } => {
            let re = compile(pattern, "replace")?;
            Ok(re.replace_all(&input, replacement.as_str()).into_owned())
        }
        Stage::KeepLines { patterns } => {
            let res = compile_patterns("keep_lines", patterns)?;
            Ok(input
                .lines()
                .filter(|l| res.iter().any(|r| r.is_match(l)))
                .collect::<Vec<_>>()
                .join("\n"))
        }
        Stage::StripLines { patterns } => {
            let res = compile_patterns("strip_lines", patterns)?;
            Ok(input
                .lines()
                .filter(|l| !res.iter().any(|r| r.is_match(l)))
                .collect::<Vec<_>>()
                .join("\n"))
        }
        Stage::HeadLines { n } => Ok(take_lines(&input, *n, LineBound::Head)),
        Stage::TailLines { n } => Ok(take_lines(&input, *n, LineBound::Tail)),
        Stage::MaxLines { n } => Ok(take_lines(&input, *n, LineBound::Head)),
    }
}

fn compile_patterns(
    stage: &'static str,
    patterns: &[String],
) -> Result<Vec<regex::Regex>, PipelineError> {
    if patterns.is_empty() {
        return Err(PipelineError::Stage {
            stage,
            reason: "empty pattern list".to_string(),
        });
    }
    patterns.iter().map(|p| compile(p, stage)).collect()
}

/// Compile one pattern with the pipeline's flag conventions.
///
/// **`multi_line(true)` is always on.** A def author writing
/// `^(\w+) = (\d+)$` means "one assignment per line" — the same
/// reading a `sed`/`grep` author would have — not "the first line
/// starts and the last line ends". Rust's regex crate defaults to
/// whole-text anchors; a text pipeline wants per-line anchors. An
/// author who genuinely wants whole-text anchors writes `\A` / `\z`
/// explicitly, which are unaffected by the flag.
fn compile(pattern: &str, stage: &'static str) -> Result<regex::Regex, PipelineError> {
    regex::RegexBuilder::new(pattern)
        .multi_line(true)
        .build()
        .map_err(|e| PipelineError::Stage {
            stage,
            reason: format!("bad pattern `{pattern}`: {e}"),
        })
}

enum LineBound {
    Head,
    Tail,
}

fn take_lines(text: &str, n: usize, bound: LineBound) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= n {
        return text.to_string();
    }
    match bound {
        LineBound::Head => lines[..n].join("\n"),
        LineBound::Tail => lines[lines.len() - n..].join("\n"),
    }
}

/// Strip ANSI SGR (colour/style) escape sequences. The bytes are
/// `\x1b[` followed by semicolon-separated decimal parameters and a
/// final `m`. The state machine handles two-character escapes
/// (`\x1b[K`, `\x1b[2J`, etc.) by dropping the whole CSI sequence —
/// the pattern `\x1b\[[0-9;]*[A-Za-z]` is the standard SGR matcher and
/// covers both cases.
pub fn strip_ansi(text: &str) -> String {
    // A regex per call is fine here: the pipeline runs once per tool
    // call, not once per line, and the pattern compiles to a
    // state machine that runs in one pass.
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]").expect("ANSI regex compiles")
    });
    re.replace_all(text, "").into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_ansi_removes_sgr() {
        assert_eq!(strip_ansi("\u{1b}[31mred\u{1b}[0m"), "red");
        assert_eq!(strip_ansi("plain"), "plain");
        assert_eq!(strip_ansi("\u{1b}[1;32mbold green\u{1b}[m"), "bold green");
    }

    #[test]
    fn strip_ansi_removes_cursor_sequences() {
        // `\x1b[K` clears to end of line; `\x1b[2J` clears the
        // screen. Both are CSI sequences and both go.
        assert_eq!(strip_ansi("a\u{1b}[Kb"), "ab");
        assert_eq!(strip_ansi("\u{1b}[2Jtop"), "top");
    }

    #[test]
    fn head_lines_keeps_the_first_n() {
        let s = run(
            &[Stage::HeadLines { n: 2 }],
            "a\nb\nc\nd",
        )
        .unwrap();
        assert_eq!(s, "a\nb");
    }

    #[test]
    fn tail_lines_keeps_the_last_n() {
        let s = run(&[Stage::TailLines { n: 2 }], "a\nb\nc\nd").unwrap();
        assert_eq!(s, "c\nd");
    }

    #[test]
    fn max_lines_is_head() {
        let s = run(&[Stage::MaxLines { n: 2 }], "a\nb\nc").unwrap();
        assert_eq!(s, "a\nb");
    }

    #[test]
    fn a_short_input_is_unchanged_by_a_line_cap() {
        let s = run(&[Stage::MaxLines { n: 100 }], "a\nb").unwrap();
        assert_eq!(s, "a\nb");
    }

    #[test]
    fn keep_lines_keeps_only_matches() {
        let s = run(
            &[Stage::KeepLines {
                patterns: vec!["^modified:".to_string()],
            }],
            "modified: a\nuntracked: b\nmodified: c",
        )
        .unwrap();
        assert_eq!(s, "modified: a\nmodified: c");
    }

    #[test]
    fn strip_lines_drops_matches() {
        let s = run(
            &[Stage::StripLines {
                patterns: vec!["^\\s*\\(use ".to_string()],
            }],
            "On branch main\n  (use \"git add\")\nmodified: a",
        )
        .unwrap();
        assert_eq!(s, "On branch main\nmodified: a");
    }

    #[test]
    fn replace_rewrites_by_regex() {
        let s = run(
            &[Stage::Replace {
                pattern: r"^  ".to_string(),
                replacement: "".to_string(),
            }],
            "  indented\nplain",
        )
        .unwrap();
        assert_eq!(s, "indented\nplain");
    }

    #[test]
    fn replace_can_use_capture_groups() {
        // `^` and `$` mean per-line (multi_line is on). The whole
        // point of the flag: a def author writing an anchor expects
        // the sed/grep reading.
        let s = run(
            &[Stage::Replace {
                pattern: r"^(\w+) = (\d+)$".to_string(),
                replacement: "$1=$2".to_string(),
            }],
            "x = 5\ny = 6",
        )
        .unwrap();
        assert_eq!(s, "x=5\ny=6");
    }

    #[test]
    fn replace_only_touches_the_matching_lines() {
        let s = run(
            &[Stage::Replace {
                pattern: r"^modified:\s+(.+)$".to_string(),
                replacement: "M $1".to_string(),
            }],
            "On branch main\nmodified:   src/lib.rs\nuntracked: foo",
        )
        .unwrap();
        assert_eq!(s, "On branch main\nM src/lib.rs\nuntracked: foo");
    }

    #[test]
    fn keep_lines_anchors_are_per_line() {
        // `^\d+$` matches a line that is only digits, anywhere in
        // the input — not just "the whole text is one number".
        let s = run(
            &[Stage::KeepLines {
                patterns: vec![r"^\d+$".to_string()],
            }],
            "one\n42\nthree\n99\n",
        )
        .unwrap();
        assert_eq!(s, "42\n99");
    }

    #[test]
    fn a_bad_replace_pattern_errors() {
        let e = run(
            &[Stage::Replace {
                pattern: "(".to_string(),
                replacement: "x".to_string(),
            }],
            "text",
        )
        .unwrap_err();
        assert!(matches!(e, PipelineError::Stage { stage: "replace", .. }));
    }

    #[test]
    fn an_empty_keep_lines_errors() {
        let e = run(&[Stage::KeepLines { patterns: vec![] }], "x").unwrap_err();
        assert!(matches!(
            e,
            PipelineError::Stage {
                stage: "keep_lines",
                ..
            }
        ));
    }

    #[test]
    fn a_def_parses_from_toml() {
        let d = Def::from_toml(
            r#"
schema_version = 1
id = "test"
program = "git"
subcommands = ["status"]
only_on_exit = 0
stages = [
    { kind = "strip_ansi" },
    { kind = "max_lines", n = 10 },
]
"#,
        )
        .unwrap();
        assert_eq!(d.id, "test");
        assert_eq!(d.program, "git");
        assert_eq!(d.subcommands, vec!["status".to_string()]);
        assert_eq!(d.only_on_exit, Some(0));
        assert_eq!(d.stages.len(), 2);
    }

    #[test]
    fn a_def_with_wrong_schema_version_errors() {
        let e = Def::from_toml(
            r#"
schema_version = 2
id = "x"
program = "x"
stages = []
"#,
        )
        .unwrap_err();
        assert!(matches!(e, DefError::Parse { .. }));
    }

    #[test]
    fn a_def_with_bad_toml_errors() {
        let e = Def::from_toml("this is not toml").unwrap_err();
        assert!(matches!(e, DefError::Parse { .. }));
    }

    #[test]
    fn matches_requires_the_program() {
        let d = Def::from_toml(
            r#"
schema_version = 1
id = "x"
program = "git"
subcommands = ["status"]
stages = []
"#,
        )
        .unwrap();
        assert!(d.matches("git", &["status".to_string()]));
        assert!(!d.matches("git", &["log".to_string()]));
        assert!(!d.matches("hg", &["status".to_string()]));
    }

    #[test]
    fn matches_with_no_subcommands_accepts_any_args() {
        let d = Def::from_toml(
            r#"
schema_version = 1
id = "x"
program = "mytool"
stages = []
"#,
        )
        .unwrap();
        assert!(d.matches("mytool", &[]));
        assert!(d.matches("mytool", &["anything".to_string()]));
    }

    #[test]
    fn the_builtin_git_status_def_parses() {
        // The include_str! would fail to compile if the file were
        // missing; this test proves the content parses.
        let (_name, text) = crate::BUILTIN_DEFS[0];
        let d = Def::from_toml(text).unwrap();
        assert_eq!(d.id, "git-status");
    }
}
