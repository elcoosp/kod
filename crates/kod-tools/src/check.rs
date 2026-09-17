//! `check`: run the project's compiler or linter and return structured
//! diagnostics.
//!
//! The agent already has `execute_command`, which can run `cargo check`.
//! What it lacks is a *structured* view of the result: the raw compiler
//! output is prose, and a model that just wrote 200 lines has to re-read
//! all of it to find the three errors it introduced. `check` runs the
//! right command for the project (Cargo, tsc, ruff, go vet), parses the
//! output, and returns a JSON list of `{file, line, column, severity,
//! message}` diagnostics the model can iterate over.
//!
//! The interface is project-agnostic: same parameters, same result
//! shape, regardless of language. Adding a new project kind is a new
//! match arm in `ProjectKind::detect` and a new parser function.

use crate::{Tool, ToolContext};
use kod_error::{KodError, Result};
use kod_types::{ToolCategory, ToolDefinition, ToolId, ToolPermissions, ToolResult};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Byte cap on each output stream we keep. A workspace with a hundred
/// errors can produce hundreds of KB of compiler output; the cap keeps
/// the tool result bounded.
const MAX_OUTPUT_BYTES: usize = 256 * 1024;

/// Wall-clock cap on the check. Long enough that a fresh `cargo check`
/// on a medium workspace completes; short enough that a hung build does
/// not stall the tool loop.
const DEFAULT_TIMEOUT_SECS: u64 = 120;

/// One structured diagnostic.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Diagnostic {
    /// Path as reported by the tool — usually relative to the workspace
    /// root, sometimes absolute. Not canonicalized; the model resolves
    /// it against the working directory.
    pub file: String,
    pub line: u32,
    pub column: u32,
    /// `error` or `warning`.
    pub severity: String,
    /// Tool-specific code when present (`E0308` for rustc, `TS2322` for
    /// tsc, `E501` for ruff). `None` when the tool does not emit codes
    /// or the line has none.
    pub code: Option<String>,
    /// The diagnostic message, verbatim.
    pub message: String,
}

/// What kind of project are we in?
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectKind {
    Cargo,
    NodeTsc,
    Ruff,
    Go,
}

impl ProjectKind {
    /// Detect from the directory contents. Order matters: a directory
    /// with both `Cargo.toml` and `package.json` (a Rust project with a
    /// TypeScript SDK, say) is treated as Rust — the outer language is
    /// the one the user is most likely asking about.
    pub fn detect(dir: &Path) -> Option<Self> {
        if dir.join("Cargo.toml").is_file() {
            Some(ProjectKind::Cargo)
        } else if dir.join("go.mod").is_file() {
            Some(ProjectKind::Go)
        } else if dir.join("pyproject.toml").is_file() || dir.join("ruff.toml").is_file() {
            Some(ProjectKind::Ruff)
        } else if dir.join("package.json").is_file() {
            Some(ProjectKind::NodeTsc)
        } else {
            None
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            ProjectKind::Cargo => "cargo",
            ProjectKind::NodeTsc => "tsc",
            ProjectKind::Ruff => "ruff",
            ProjectKind::Go => "go",
        }
    }

    /// The command to run: `(program, args)`.
    pub fn command(&self) -> (&'static str, Vec<&'static str>) {
        match self {
            // `--message-format=short` gives one diagnostic per line:
            //   path:line:col: error[E0308]: message
            // Much easier to parse than cargo's default JSON-ish form.
            ProjectKind::Cargo => {
                ("cargo", vec!["check", "--message-format=short", "--quiet"])
            }
            // tsc emits `path(line,col): error TSxxxx: message` per line.
            ProjectKind::NodeTsc => ("npx", vec!["--no-install", "tsc", "--noEmit"]),
            // ruff's concise format: `path:line:col: CODE message`.
            ProjectKind::Ruff => ("ruff", vec!["check", "--output-format=concise"]),
            // `go vet` prints `path:line:col: message`.
            ProjectKind::Go => ("go", vec!["vet", "./..."]),
        }
    }

    /// Parse a single line of the tool's output into a diagnostic, or
    /// `None` when the line is not a diagnostic (a header, a summary, a
    /// blank line).
    pub fn parse_line(&self, line: &str) -> Option<Diagnostic> {
        match self {
            ProjectKind::Cargo => parse_rust_line(line),
            ProjectKind::NodeTsc => parse_tsc_line(line),
            ProjectKind::Ruff => parse_ruff_line(line),
            ProjectKind::Go => parse_go_line(line),
        }
    }
}

/// `path:line:col: error[CODE]: message`
/// `path:line:col: warning: message`
fn parse_rust_line(line: &str) -> Option<Diagnostic> {
    let (head, rest) = split_at_severity(line, &["error", "warning"])?;
    let (file, line_no, col_no) = parse_location_colon(head)?;
    let (severity, code, message) = parse_severity_and_code(rest)?;
    Some(Diagnostic {
        file,
        line: line_no,
        column: col_no,
        severity,
        code,
        message,
    })
}

/// `path(line,col): error TSxxxx: message`
fn parse_tsc_line(line: &str) -> Option<Diagnostic> {
    let open = line.find('(')?;
    let close = line[open..].find(')')? + open;
    let file = line[..open].to_string();
    let location = &line[open + 1..close];
    let mut parts = location.split(',');
    let line_no: u32 = parts.next()?.trim().parse().ok()?;
    let col_no: u32 = parts.next()?.trim().parse().ok()?;
    let rest = line[close + 1..].trim_start_matches(':').trim_start();
    let (severity, _code, message) = parse_severity_and_code(rest)?;
    // tsc puts the code (`TS2322`) at the start of the message rather
    // than in a bracket. Split it out here.
    let (code, message) = if let Some((first, tail)) = message.split_once(':') {
        if first.starts_with("TS") {
            (Some(first.to_string()), tail.trim_start().to_string())
        } else {
            (None, message)
        }
    } else {
        (None, message)
    };
    Some(Diagnostic {
        file,
        line: line_no,
        column: col_no,
        severity,
        code,
        message,
    })
}

/// `path:line:col: CODE message` (ruff concise).
///
/// Ruff's concise output has no severity word: every finding is
/// prefixed by an uppercase code (`E501`, `F401`, `I001`, ...) rather
/// than by `error`/`warning`. The code is treated as the diagnostic
/// name; everything after it is the message. When the code is missing
/// (a few ruff rules emit plain text), the whole tail is the message.
fn parse_ruff_line(line: &str) -> Option<Diagnostic> {
    // `path:line:col: <rest>` — split off the location by scanning
    // for the third colon.
    let (file, line_no, col_no, rest) = parse_ruff_prefix(line)?;
    let rest = rest.trim();
    if rest.is_empty() {
        return None;
    }
    // A ruff code is all-uppercase letters and digits, at least two
    // chars, and starts with an uppercase letter. `E501` matches;
    // `Line` (the start of a plain message) does not.
    let (code, message) = match rest.split_once(' ') {
        Some((first, tail))
            if first.len() >= 2
                && first.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
                && first.chars().next().is_some_and(|c| c.is_ascii_uppercase()) =>
        {
            (Some(first.to_string()), tail.trim().to_string())
        }
        _ => (None, rest.to_string()),
    };
    Some(Diagnostic {
        file,
        line: line_no,
        column: col_no,
        // Ruff exits 1 on findings; every finding is actionable.
        severity: "error".to_string(),
        code,
        message,
    })
}

/// Split a `path:line:col: rest` line. `path` may itself contain a
/// colon (Windows drive letters, URLs in an annotation), so the split
/// scans from the left for the *third* colon that is followed by
/// whitespace or content, skipping the drive letter when present.
fn parse_ruff_prefix(line: &str) -> Option<(String, u32, u32, &str)> {
    let bytes = line.as_bytes();
    let mut i = 0;
    // Skip a `C:` style drive letter.
    if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        i = 2;
    }
    let mut colons = Vec::new();
    while i < bytes.len() {
        if bytes[i] == b':' {
            colons.push(i);
            if colons.len() == 3 {
                break;
            }
        }
        i += 1;
    }
    if colons.len() < 3 {
        return None;
    }
    let file = line[..colons[0]].to_string();
    let line_no: u32 = line[colons[0] + 1..colons[1]].trim().parse().ok()?;
    let col_no: u32 = line[colons[1] + 1..colons[2]].trim().parse().ok()?;
    let rest = &line[colons[2] + 1..];
    Some((file, line_no, col_no, rest))
}

/// `path:line:col: message`
fn parse_go_line(line: &str) -> Option<Diagnostic> {
    let (file, line_no, col_no) = parse_location_colon_prefix(line)?;
    let mut colons = 0;
    let mut msg_start = line.len();
    for (i, c) in line.char_indices() {
        if c == ':' {
            colons += 1;
            if colons == 3 {
                msg_start = i + 1;
                break;
            }
        }
    }
    let message = line[msg_start..].trim().to_string();
    if message.is_empty() {
        return None;
    }
    Some(Diagnostic {
        file,
        line: line_no,
        column: col_no,
        severity: "error".to_string(),
        code: None,
        message,
    })
}

/// Find the first `: error` or `: warning` and split around it. The
/// suffix begins at the severity word (`error`/`warning`), not at the
/// colon.
fn split_at_severity<'a>(line: &'a str, sevs: &[&str]) -> Option<(&'a str, &'a str)> {
    for sev in sevs {
        let needle = format!(": {sev}");
        if let Some(pos) = line.find(&needle) {
            return Some((&line[..pos], &line[pos + 2..]));
        }
    }
    None
}

/// `file:line:col` → `(file, line, col)`. The file part may itself
/// contain colons (Windows drive letters), so split from the right.
fn parse_location_colon(s: &str) -> Option<(String, u32, u32)> {
    let mut parts = s.rsplitn(3, ':');
    let col: u32 = parts.next()?.trim().parse().ok()?;
    let line: u32 = parts.next()?.trim().parse().ok()?;
    let file = parts.next()?.trim().to_string();
    if file.is_empty() {
        return None;
    }
    Some((file, line, col))
}

/// Same as `parse_location_colon` but for the go vet output, where the
/// location is at the *start* of the line. Splits on the first three
/// colons from the left after skipping a possible Windows drive letter.
fn parse_location_colon_prefix(s: &str) -> Option<(String, u32, u32)> {
    // Locate the three colons that separate file:line:col:.
    let bytes = s.as_bytes();
    let mut colons = Vec::new();
    let mut i = 0;
    // Skip `C:\` if present.
    if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        i = 2;
    }
    while i < bytes.len() {
        if bytes[i] == b':' {
            colons.push(i);
            if colons.len() == 3 {
                break;
            }
        }
        i += 1;
    }
    if colons.len() < 3 {
        return None;
    }
    let file = s[..colons[0]].to_string();
    let line: u32 = s[colons[0] + 1..colons[1]].trim().parse().ok()?;
    let col: u32 = s[colons[1] + 1..colons[2]].trim().parse().ok()?;
    Some((file, line, col))
}

/// `error[E0308]: mismatched types` → `("error", Some("E0308"), "mismatched types")`
/// `warning: unused variable` → `("warning", None, "unused variable")`
fn parse_severity_and_code(s: &str) -> Option<(String, Option<String>, String)> {
    let (first_word, rest) = match s.split_once(' ') {
        Some((a, b)) => (a, b.to_string()),
        None => (s, String::new()),
    };
    let severity = if first_word.starts_with("error") {
        "error"
    } else if first_word.starts_with("warning") {
        "warning"
    } else {
        return None;
    };
    let code = if let Some(start) = first_word.find('[')
        && let Some(end) = first_word.find(']')
    {
        Some(first_word[start + 1..end].to_string())
    } else {
        None
    };
    let trimmed = rest.trim_start_matches(':').trim_start();
    let message = trimmed.to_string();
    if message.is_empty() {
        return None;
    }
    Some((severity.to_string(), code, message))
}

pub struct CheckTool {
    pub definition: ToolDefinition,
}

impl CheckTool {
    pub fn new() -> Self {
        Self {
            definition: ToolDefinition {
                id: ToolId::new(),
                name: "check".to_string(),
                description:
                    "Run the project's compiler or linter and return structured \
                     diagnostics. Detects Cargo, tsc, ruff, or go vet from the working \
                     directory. Returns {kind, command, exit_code, diagnostic_count, \
                     diagnostics: [{file, line, column, severity, code, message}], \
                     truncated}. Use after writing or patching files to see if the change \
                     broke the build."
                        .to_string(),
                category: ToolCategory::System,
                parameters_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Optional subdirectory to check. Defaults to the working directory."
                        }
                    },
                    "additionalProperties": false
                }),
                permissions: ToolPermissions {
                    read_files: true,
                    write_files: false,
                    execute_commands: true,
                    network_access: false,
                    git_access: kod_types::GitAccess::None,
                    allowed_paths: Vec::new(),
                    forbidden_paths: Vec::new(),
                },
            },
        }
    }

    /// Run the project's check command and parse its output. Public so
    /// the engine's auto-check hook can reuse it without going through
    /// the `Tool` trait.
    pub async fn run_check(workdir: &Path, timeout_secs: u64) -> Result<CheckOutcome> {
        let kind = ProjectKind::detect(workdir).ok_or_else(|| KodError::ToolExecution {
            tool_name: "check".to_string(),
            reason: format!(
                "no recognized project at {} — look for Cargo.toml, package.json, \
                 pyproject.toml, or go.mod",
                workdir.display()
            ),
        })?;
        let (program, args) = kind.command();
        let mut cmd = tokio::process::Command::new(program);
        cmd.args(&args)
            .current_dir(workdir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        let output = match tokio::time::timeout(Duration::from_secs(timeout_secs), cmd.output())
            .await
        {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => {
                return Err(KodError::ToolExecution {
                    tool_name: "check".to_string(),
                    reason: format!(
                        "could not run `{}`: {}. Is the toolchain on PATH?",
                        program, e
                    ),
                });
            }
            Err(_) => {
                return Err(KodError::ToolExecution {
                    tool_name: "check".to_string(),
                    reason: format!("`{}` did not finish within {}s", program, timeout_secs),
                });
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Most diagnostics go to stderr for Rust and Go, to stdout for
        // tsc and ruff. Parse both, merge.
        let mut diagnostics: Vec<Diagnostic> = Vec::new();
        for line in stdout.lines().chain(stderr.lines()) {
            if let Some(d) = kind.parse_line(line) {
                diagnostics.push(d);
            }
        }
        // Dedupe identical diagnostics (cargo emits the same error
        // twice when it originates in a macro expansion).
        diagnostics.sort_by(|a, b| {
            (&a.file, a.line, a.column, &a.message).cmp(&(&b.file, b.line, b.column, &b.message))
        });
        diagnostics.dedup_by(|a, b| {
            a.file == b.file
                && a.line == b.line
                && a.column == b.column
                && a.message == b.message
        });

        let (stdout_short, stdout_truncated) = truncate(&stdout, MAX_OUTPUT_BYTES);
        let (stderr_short, stderr_truncated) = truncate(&stderr, MAX_OUTPUT_BYTES);

        Ok(CheckOutcome {
            kind: kind.name().to_string(),
            command: format!("{} {}", program, args.join(" ")),
            exit_code: output.status.code().unwrap_or(-1),
            diagnostics,
            stdout: stdout_short,
            stderr: stderr_short,
            truncated: stdout_truncated || stderr_truncated,
        })
    }
}

impl Default for CheckTool {
    fn default() -> Self {
        Self::new()
    }
}

/// Everything `run_check` produces.
#[derive(Debug, Clone)]
pub struct CheckOutcome {
    pub kind: String,
    pub command: String,
    pub exit_code: i32,
    pub diagnostics: Vec<Diagnostic>,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
}

impl CheckOutcome {
    /// Render as the JSON value the tool returns and the model reads.
    pub fn to_json(&self) -> Value {
        let diags: Vec<Value> = self
            .diagnostics
            .iter()
            .map(|d| {
                serde_json::json!({
                    "file": d.file,
                    "line": d.line,
                    "column": d.column,
                    "severity": d.severity,
                    "code": d.code,
                    "message": d.message,
                })
            })
            .collect();
        serde_json::json!({
            "kind": self.kind,
            "command": self.command,
            "exit_code": self.exit_code,
            "diagnostic_count": self.diagnostics.len(),
            "diagnostics": diags,
            "truncated": self.truncated,
        })
    }
}

fn truncate(s: &str, max: usize) -> (String, bool) {
    if s.len() <= max {
        return (s.to_string(), false);
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    (s[..end].to_string(), true)
}

#[async_trait::async_trait]
impl Tool for CheckTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    async fn execute(&self, params: &Value, context: &ToolContext) -> Result<ToolResult> {
        context.can_execute_command("check")?;
        let workdir: PathBuf = match params.get("path").and_then(|v| v.as_str()) {
            Some(p) => context.resolve_path(p)?,
            None => context.working_dir.clone(),
        };
        if !workdir.is_dir() {
            return Ok(ToolResult::Error(format!(
                "not a directory: {}",
                workdir.display()
            )));
        }
        match Self::run_check(&workdir, DEFAULT_TIMEOUT_SECS).await {
            Ok(outcome) => Ok(ToolResult::Success(outcome.to_json())),
            Err(e) => Ok(ToolResult::Error(e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_cargo() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "[package]").unwrap();
        assert_eq!(ProjectKind::detect(tmp.path()), Some(ProjectKind::Cargo));
    }

    #[test]
    fn detect_go() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("go.mod"), "module x").unwrap();
        assert_eq!(ProjectKind::detect(tmp.path()), Some(ProjectKind::Go));
    }

    #[test]
    fn detect_none_for_empty_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert_eq!(ProjectKind::detect(tmp.path()), None);
    }

    #[test]
    fn parse_rust_error_with_code() {
        let line = "src/main.rs:42:5: error[E0308]: mismatched types";
        let d = parse_rust_line(line).unwrap();
        assert_eq!(d.file, "src/main.rs");
        assert_eq!(d.line, 42);
        assert_eq!(d.column, 5);
        assert_eq!(d.severity, "error");
        assert_eq!(d.code.as_deref(), Some("E0308"));
        assert_eq!(d.message, "mismatched types");
    }

    #[test]
    fn parse_rust_warning_no_code() {
        let line = "src/main.rs:10:13: warning: unused variable: `x`";
        let d = parse_rust_line(line).unwrap();
        assert_eq!(d.severity, "warning");
        assert_eq!(d.code, None);
        assert_eq!(d.message, "unused variable: `x`");
    }

    #[test]
    fn parse_rust_line_rejects_headers() {
        assert!(parse_rust_line("   Compiling kod-tools v0.1.0").is_none());
        assert!(parse_rust_line("    Finished `dev` profile").is_none());
        assert!(parse_rust_line("").is_none());
    }

    #[test]
    fn parse_tsc_error() {
        let line =
            "src/foo.ts(42,5): error TS2322: Type 'string' is not assignable to type 'number'.";
        let d = parse_tsc_line(line).unwrap();
        assert_eq!(d.file, "src/foo.ts");
        assert_eq!(d.line, 42);
        assert_eq!(d.column, 5);
        assert_eq!(d.severity, "error");
        assert_eq!(d.code.as_deref(), Some("TS2322"));
        assert_eq!(
            d.message,
            "Type 'string' is not assignable to type 'number'."
        );
    }

    #[test]
    fn parse_ruff_line_with_code() {
        let line = "src/foo.py:42:5: E501 Line too long (92 > 88)";
        let d = parse_ruff_line(line).unwrap();
        assert_eq!(d.file, "src/foo.py");
        assert_eq!(d.line, 42);
        assert_eq!(d.column, 5);
        assert_eq!(d.code.as_deref(), Some("E501"));
        assert_eq!(d.message, "Line too long (92 > 88)");
    }

    #[test]
    fn parse_go_line_test() {
        let line =
            "src/foo.go:42:5: fmt.Printf format %d has arg s of wrong type string";
        let d = parse_go_line(line).unwrap();
        assert_eq!(d.file, "src/foo.go");
        assert_eq!(d.line, 42);
        assert_eq!(d.column, 5);
        assert_eq!(
            d.message,
            "fmt.Printf format %d has arg s of wrong type string"
        );
    }

    #[test]
    fn parse_location_handles_windows_drive_letters() {
        // A Windows absolute path must survive `parse_location_colon`
        // even though it contains a colon at index 1 (after the drive
        // letter). Split-from-the-right handles it correctly.
        let (file, line, col) = parse_location_colon(r"C:\src\main.rs:42:5").unwrap();
        assert_eq!(file, r"C:\src\main.rs");
        assert_eq!(line, 42);
        assert_eq!(col, 5);
    }

    #[tokio::test]
    async fn run_check_reports_no_project() {
        let tmp = tempfile::TempDir::new().unwrap();
        let err = CheckTool::run_check(tmp.path(), 10).await.unwrap_err();
        assert!(err.to_string().contains("no recognized project"));
    }
}
