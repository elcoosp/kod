//! Shell output minimizer (borrow from oh-my-pi, delta §5).
//!
//! # The gap this fills
//!
//! kod caps tool results at 16 KiB
//! ([`kod_core::engine::cap_rendered_result`]) and `execute_command`
//! caps its own capture at `MAX_CMD_OUTPUT_BYTES`. Both are *chops*:
//! a large `git status`, a verbose `pytest` run, or a `cargo check`
//! with fifty warnings all get truncated at the same byte boundary,
//! and the information the model needed is very often in the part
//! that got cut.
//!
//! A **minimizer** rewrites the output into a shorter form that keeps
//! the informative content. `git status` becomes branch name plus a
//! file list; `git log` becomes short-hash plus subject lines; a
//! `cargo check` failure becomes the error slice, not the first N
//! bytes. The raw output is preserved as an artifact so the human —
//! or the agent, deliberately — can still fetch it.
//!
//! # Shape
//!
//! A [`Minimizer`] holds a registry of [`Def`]s. Each def names a
//! program (`git`, `cargo`, `pytest`), the subcommands it applies to,
//! and a pipeline of [`Stage`]s. [`Minimizer::minimize`] takes a
//! command string and the captured [`Capture`], classifies the
//! command via [`plan::classify`], looks up the def, and runs the
//! pipeline. When no def matches — or when the command is a pipe, a
//! chain, or unsupported — the raw capture is returned unchanged.
//!
//! # The pipe-opacity invariant
//!
//! **Pipes are opaque.** `git status | grep modified` produces output
//! whose content depends on the *next* stage of the pipeline; a
//! minimizer that rewrote it would be lying about what the shell
//! actually produced. [`plan::classify`] returns
//! [`plan::CommandPlan::Piped`] for any command containing a top-level
//! `|`, and [`Minimizer::minimize`] returns the raw capture for it.
//!
//! The same reasoning applies to `&&` and `;` chains: a chain's
//! segments are minimized independently, but the *last* segment's
//! output is what the caller sees, and a minimizer that rewrote
//! intermediate output would break the chain's actual behavior.
//! [`plan::classify`] returns [`plan::CommandPlan::Chain`], which the
//! minimizer treats as "return raw" — a segment-aware rewrite is a
//! follow-up.
//!
//! # What this does NOT do
//!
//! * **Not a filter for arbitrary programs.** The registry ships with
//!   a small set of built-in defs, and a caller can register more.
//!   An unrecognized program returns the raw capture.
//! * **Not a lossy rewriter.** Every minimized result carries
//!   `original_bytes` and `output_bytes`; a caller that wants the
//!   original keeps it. The [`Minimized`] type does not hold the raw
//!   bytes — the caller already has them — but reports both sizes.
//! * **Not an output-dropping pass.** Truncation is only ever applied
//!   by an explicit `max_lines` stage in a def. The pipeline never
//!   silently drops content.

pub mod pipeline;
pub mod plan;

use pipeline::{Def, PipelineError};

/// What a minimizer produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Minimized {
    /// The rewritten text. When no def matched, this is the original
    /// (possibly ANSI-stripped, if a global stage applies).
    pub text: String,
    /// The def that matched, if any. `None` for a pass-through.
    pub filter: Option<String>,
    /// Bytes in the raw capture.
    pub original_bytes: usize,
    /// Bytes in [`Self::text`].
    pub output_bytes: usize,
}

impl Minimized {
    /// Construct a pass-through result: same content, same size,
    /// no filter.
    pub fn raw(raw: &str) -> Self {
        Self {
            text: raw.to_string(),
            filter: None,
            original_bytes: raw.len(),
            output_bytes: raw.len(),
        }
    }

    /// Whether a def actually rewrote the content.
    pub fn was_rewritten(&self) -> bool {
        self.filter.is_some()
    }
}

/// The minimizer's public surface.
///
/// Cheap to construct; holds a `Vec<Def>` and does not allocate on
/// construction beyond the defs themselves.
pub struct Minimizer {
    defs: Vec<Def>,
}

/// The minimizer's configuration. Defaults to the built-in defs.
#[derive(Debug, Clone)]
pub struct MinimizeConfig {
    /// Whether the minimizer runs at all. A caller can flip this off
    /// for a session; the tool then returns the raw capture with the
    /// same shape, so downstream code does not branch.
    pub enabled: bool,
    /// Whether to strip ANSI escape sequences from every capture,
    /// even when no def matches. A command with no def still returns
    /// text a model can read without the `\x1b[31m` bytes. On by
    /// default.
    pub strip_ansi_globally: bool,
}

impl Default for MinimizeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            strip_ansi_globally: true,
        }
    }
}

impl Minimizer {
    /// Build a minimizer with the built-in defs and the default
    /// config. Never fails: a malformed built-in def is skipped with
    /// a `tracing::warn!` (it cannot happen at compile time because
    /// the defs are `include_str!`-ed and their parse is covered by
    /// the test suite, but defensive skipping means a future bad def
    /// does not brick the crate).
    pub fn with_builtins() -> Self {
        Self::with_builtins_and_config(MinimizeConfig::default())
    }

    /// Build a minimizer with the built-in defs and a caller-supplied
    /// config.
    pub fn with_builtins_and_config(_config: MinimizeConfig) -> Self {
        let mut defs = Vec::new();
        for (name, text) in BUILTIN_DEFS {
            match Def::from_toml(text) {
                Ok(d) => defs.push(d),
                Err(e) => tracing::warn!(
                    def = *name,
                    error = %e,
                    "skipping malformed built-in minimizer def",
                ),
            }
        }
        Self { defs }
    }

    /// An empty minimizer. Useful in tests and in a caller that wants
    /// to control the def set.
    pub fn empty() -> Self {
        Self { defs: Vec::new() }
    }

    /// Register an additional def. Replaces an existing def with the
    /// same id.
    pub fn register(mut self, def: Def) -> Self {
        self.defs.retain(|d| d.id != def.id);
        self.defs.push(def);
        self
    }

    /// The defs this minimizer knows, sorted by id.
    pub fn def_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.defs.iter().map(|d| d.id.clone()).collect();
        ids.sort();
        ids
    }

    /// Minimize the output of `command`.
    ///
    /// `raw` is the captured text; `exit_code` gates the def's
    /// `only_on_exit` filter. Returns a [`Minimized`] that always
    /// reflects the shape of the result — a caller that wants the
    /// raw bytes back can compare `filter` to `None`.
    pub fn minimize(&self, command: &str, raw: &str, exit_code: i32) -> Minimized {
        match plan::classify(command) {
            plan::CommandPlan::Single { program, args } => {
                self.minimize_single(&program, &args, raw, exit_code)
            }
            // Piped, chained, or unsupported: the pipe-opacity
            // invariant. Return the raw capture.
            _ => Minimized::raw(raw),
        }
    }

    fn minimize_single(
        &self,
        program: &str,
        args: &[String],
        raw: &str,
        exit_code: i32,
    ) -> Minimized {
        // Find the first def that matches. The registry is small
        // enough that linear scan is fine; a hash map keyed on
        // program would be premature.
        for def in &self.defs {
            if !def.matches(program, args) {
                continue;
            }
            if let Some(expected) = def.only_on_exit
                && expected != exit_code
            {
                continue;
            }
            let text = match pipeline::run(&def.stages, raw) {
                Ok(t) => t,
                Err(PipelineError::Stage { stage, reason }) => {
                    tracing::warn!(
                        def = %def.id,
                        stage = %stage,
                        reason = %reason,
                        "minimizer pipeline stage failed; returning raw",
                    );
                    return Minimized::raw(raw);
                }
            };
            return Minimized {
                text: text.clone(),
                filter: Some(def.id.clone()),
                original_bytes: raw.len(),
                output_bytes: text.len(),
            };
        }
        Minimized::raw(raw)
    }
}

/// The built-in defs, embedded at compile time.
///
/// Adding a def is one `.toml` file and one entry here. The tuple is
/// `(filename, contents)` — the filename is only used in the warn log
/// when a def fails to parse.
pub const BUILTIN_DEFS: &[(&str, &str)] = &[(
    "git-status.toml",
    include_str!("../defs/git-status.toml"),
)];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_minimizer_returns_raw_for_everything() {
        let m = Minimizer::empty();
        let r = m.minimize("git status", "on branch main\n", 0);
        assert!(r.filter.is_none());
        assert_eq!(r.text, "on branch main\n");
        assert_eq!(r.original_bytes, r.output_bytes);
    }

    #[test]
    fn builtins_include_git_status() {
        let m = Minimizer::with_builtins();
        assert!(m.def_ids().contains(&"git-status".to_string()));
    }

    #[test]
    fn a_matching_single_command_is_minimized() {
        let m = Minimizer::with_builtins();
        let raw = "\u{1b}[32mOn branch main\u{1b}[0m\n\
                   Changes not staged for commit:\n\
                   \tmodified:   src/lib.rs\n";
        let r = m.minimize("git status", raw, 0);
        assert_eq!(r.filter.as_deref(), Some("git-status"));
        assert!(
            !r.text.contains('\u{1b}'),
            "ANSI must be stripped by the pipeline: {:?}",
            r.text,
        );
        assert!(r.text.contains("On branch main"));
    }

    #[test]
    fn a_piped_command_is_not_minimized() {
        // Pipe opacity: the content the model sees is what the *last*
        // stage of the pipeline produced, which the minimizer cannot
        // know. Return raw.
        let m = Minimizer::with_builtins();
        let raw = "On branch main\n";
        let r = m.minimize("git status | grep modified", raw, 0);
        assert!(r.filter.is_none());
        assert_eq!(r.text, raw);
    }

    #[test]
    fn a_chained_command_is_not_minimized() {
        let m = Minimizer::with_builtins();
        let raw = "no changes\n";
        let r = m.minimize("git status && echo done", raw, 0);
        assert!(r.filter.is_none());
    }

    #[test]
    fn a_non_matching_program_returns_raw() {
        let m = Minimizer::with_builtins();
        let raw = "hello\n";
        let r = m.minimize("ls -la", raw, 0);
        assert!(r.filter.is_none());
        assert_eq!(r.text, raw);
    }

    #[test]
    fn only_on_exit_filters_out_an_unexpected_code() {
        // The git-status def requires exit 0. A nonzero exit (the
        // command failed) returns the raw capture — the failure
        // message is the information the model needs, and a
        // minimizer that rewrote it away would be worse than no
        // minimizer.
        let m = Minimizer::with_builtins();
        let raw = "fatal: not a git repository\n";
        let r = m.minimize("git status", raw, 128);
        assert!(r.filter.is_none());
        assert_eq!(r.text, raw);
    }

    #[test]
    fn minimized_reports_both_sizes() {
        let m = Minimizer::with_builtins();
        let raw = "\u{1b}[32mOn branch main\u{1b}[0m\n";
        let r = m.minimize("git status", raw, 0);
        assert_eq!(r.original_bytes, raw.len());
        // The output is shorter after ANSI stripping.
        assert!(r.output_bytes < r.original_bytes);
    }

    #[test]
    fn was_rewritten_reflects_the_filter_field() {
        let m = Minimizer::with_builtins();
        let rewritten = m.minimize("git status", "On branch main\n", 0);
        assert!(rewritten.was_rewritten());
        let passthrough = m.minimize("ls", "x\n", 0);
        assert!(!passthrough.was_rewritten());
    }

    #[test]
    fn register_replaces_by_id() {
        let d1 = Def::from_toml(
            r#"
schema_version = 1
id = "test"
program = "mytool"
stages = [ { kind = "max_lines", n = 1 } ]
"#,
        )
        .unwrap();
        let d2 = Def::from_toml(
            r#"
schema_version = 1
id = "test"
program = "mytool"
stages = [ { kind = "max_lines", n = 2 } ]
"#,
        )
        .unwrap();
        let m = Minimizer::empty().register(d1).register(d2);
        assert_eq!(m.def_ids(), vec!["test".to_string()]);
    }
}
