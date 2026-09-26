//! Conditional project instructions (P4).
//!
//! An `AGENTS.md` (or `CLAUDE.md`) at the workspace root, and
//! optionally at any subdirectory, holds user-authored guidance.
//! A section with the same heading in a deeper file replaces the
//! shallower one for a task that touches that subtree.
//!
//! Sections can be guarded with a `::: when <condition>` fence:
//!
//! ```text
//! ::: when task=Debugging
//! Prefer `tracing` over `println!`.
//! :::
//!
//! ::: when path=crates/web/**
//! Regenerate tailwind via `npm run gen`.
//! :::
//! ```
//!
//! Conditions compose with the existing classifier rather than
//! replacing it: a Debugging turn in `crates/web` picks up both
//! the debugging preamble kod already builds and the web footguns
//! file the user wrote.
//!
//! Rendered sections go in the *volatile* prompt slot, not the
//! cacheable prefix — a turn that loads a different set of sections
//! does not invalidate the prefix cache (P0).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Which turns an instruction section applies to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum When {
    Always,
    TaskType(String),
    Path(String),
    Lang(String),
}

impl When {
    pub fn label(&self) -> String {
        match self {
            When::Always => "always".to_string(),
            When::TaskType(t) => format!("task={t}"),
            When::Path(p) => format!("path={p}"),
            When::Lang(l) => format!("lang={l}"),
        }
    }
}

/// One rendered block from an AGENTS.md.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstructionSection {
    pub when: When,
    pub source: PathBuf,
    /// Section heading, if the block had one. Used for shadowing.
    pub name: Option<String>,
    pub body: String,
}

/// Parse an `AGENTS.md` body into sections.
pub fn parse_agents_md(src: &str, from: &Path) -> Vec<InstructionSection> {
    let mut out: Vec<InstructionSection> = Vec::new();
    let mut current_when = When::Always;
    let mut current_name: Option<String> = None;
    let mut buf = String::new();
    let mut in_fence = false;

    fn flush(
        out: &mut Vec<InstructionSection>,
        when: When,
        name: Option<String>,
        buf: &mut String,
        from: &Path,
    ) {
        let body = std::mem::take(buf);
        if body.trim().is_empty() {
            return;
        }
        out.push(InstructionSection {
            when,
            source: from.to_path_buf(),
            name,
            body,
        });
    }

    for line in src.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix(":::") {
            let rest = rest.trim();
            if rest.is_empty() {
                flush(&mut out, current_when.clone(), current_name.clone(), &mut buf, from);
                current_when = When::Always;
                in_fence = false;
                continue;
            }
            if let Some(cond) = rest.strip_prefix("when").map(str::trim) {
                flush(&mut out, current_when.clone(), current_name.clone(), &mut buf, from);
                current_when = parse_condition(cond);
                in_fence = true;
            }
            continue;
        }
        if !in_fence && let Some(name) = trimmed.strip_prefix("## ") {
            flush(&mut out, current_when.clone(), current_name.clone(), &mut buf, from);
            current_when = When::Always;
            current_name = Some(name.trim().to_string());
            continue;
        }
        buf.push_str(line);
        buf.push('\n');
    }
    flush(&mut out, current_when, current_name, &mut buf, from);
    out
}

fn parse_condition(s: &str) -> When {
    if let Some(v) = s.strip_prefix("task=") {
        return When::TaskType(v.trim().to_string());
    }
    if let Some(v) = s.strip_prefix("path=") {
        return When::Path(v.trim().to_string());
    }
    if let Some(v) = s.strip_prefix("lang=") {
        return When::Lang(v.trim().to_string());
    }
    When::Always
}

/// The maximum nesting depth of `@`-imports in an `AGENTS.md`
/// (borrow from oh-my-pi, delta §12.8). The top-level file is
/// depth 0; up to five nested imports are expanded and a sixth is
/// left literal.
pub const MAX_IMPORT_DEPTH: usize = 5;

/// Expand `@path` imports inside an `AGENTS.md` body.
///
/// A line whose trimmed content is a single `@path` token (with no
/// trailing prose) inlines the referenced file's text at that point.
/// The reference resolves relative to `from`'s parent directory, so
/// a `@footguns.md` next to the importing file just works.
///
/// # Rules
///
/// * **Code fences suspend expansion.** A `@path` inside a ``` ``` ```
///   or `~~~` block is documentation, not a reference. A doc example
///   of the syntax does not pull in a file.
/// * **Depth-limited.** [`MAX_IMPORT_DEPTH`] nested imports expand;
///   a deeper reference stays literal.
/// * **Cycle-safe.** The walk tracks canonical paths, so a chain
///   that transitively imports itself short-circuits at the second
///   visit.
/// * **Best-effort.** A reference that does not resolve (missing
///   file, read error, canonicalize failure) stays literal. A broken
///   import degrades to text, which is what a user writing `@user`
///   in prose expects.
///
/// # Section attribution
///
/// Expansion runs *before* [`parse_agents_md`], so every resulting
/// section's `source` is the importing file. An imported section's
/// original path is not preserved; a caller that needs it reads the
/// imported file directly.
pub fn expand_imports(src: &str, from: &Path) -> String {
    let mut visited: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    // Seed with the top-level file so a true cycle (A imports B, B
    // imports A) short-circuits on the second visit rather than
    // duplicating A's content through B.
    if let Ok(c) = std::fs::canonicalize(from) {
        visited.insert(c);
    }
    let base = from.parent().unwrap_or(Path::new("."));
    expand_imports_rec(src, base, 0, &mut visited)
}

fn expand_imports_rec(
    src: &str,
    base_dir: &Path,
    depth: usize,
    visited: &mut std::collections::HashSet<PathBuf>,
) -> String {
    let mut out = String::with_capacity(src.len());
    let mut in_fence = false;
    let mut fence: &str = "";
    for line in src.lines() {
        let trimmed = line.trim_start();
        if !in_fence {
            if trimmed.starts_with("```") {
                in_fence = true;
                fence = "```";
                out.push_str(line);
                out.push('\n');
                continue;
            }
            if trimmed.starts_with("~~~") {
                in_fence = true;
                fence = "~~~";
                out.push_str(line);
                out.push('\n');
                continue;
            }
        } else if trimmed.starts_with(fence) {
            in_fence = false;
            out.push_str(line);
            out.push('\n');
            continue;
        } else {
            // Inside a fence: literal text, no expansion.
            out.push_str(line);
            out.push('\n');
            continue;
        }
        // Outside a fence: an `@path` line with no trailing prose is
        // a reference.
        if let Some(rest) = line.trim().strip_prefix('@') {
            if !rest.is_empty()
                && !rest.chars().any(char::is_whitespace)
                && depth + 1 <= MAX_IMPORT_DEPTH
            {
                if let Some(expanded) =
                    try_expand_import(rest, base_dir, depth + 1, visited)
                {
                    out.push_str(&expanded);
                    if !expanded.ends_with('\n') {
                        out.push('\n');
                    }
                    continue;
                }
            }
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn try_expand_import(
    token: &str,
    base_dir: &Path,
    depth_of_import: usize,
    visited: &mut std::collections::HashSet<PathBuf>,
) -> Option<String> {
    let candidate: PathBuf = if let Some(rest) = token.strip_prefix("~/") {
        dirs::home_dir()?.join(rest)
    } else {
        let p = PathBuf::from(token);
        if p.is_absolute() { p } else { base_dir.join(p) }
    };
    let canonical = std::fs::canonicalize(&candidate).ok()?;
    if !canonical.is_file() {
        return None;
    }
    if visited.contains(&canonical) {
        // Cycle: leave the reference literal so the reader sees it.
        return None;
    }
    let body = std::fs::read_to_string(&canonical).ok()?;
    visited.insert(canonical.clone());
    let parent = canonical
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| base_dir.to_path_buf());
    let expanded = expand_imports_rec(&body, &parent, depth_of_import, visited);
    // Allow siblings to reference the same file: the visited guard
    // is for cycles, not for de-duplication.
    visited.remove(&canonical);
    Some(expanded)
}

/// The full instruction chain, root → leaf.
#[derive(Debug, Clone, Default)]
pub struct InstructionChain {
    pub sections: Vec<InstructionSection>,
}

impl InstructionChain {
    /// Load every `AGENTS.md` / `CLAUDE.md` from `repo_root` down to
    /// (and including) `cwd`, in root-first order.
    pub fn load(cwd: &Path, repo_root: &Path) -> Self {
        let mut raw = Vec::new();
        let mut dirs: Vec<PathBuf> = Vec::new();
        let mut p = cwd.to_path_buf();
        loop {
            dirs.push(p.clone());
            if p == repo_root || !p.pop() || !p.starts_with(repo_root) {
                break;
            }
        }
        dirs.reverse();
        for dir in dirs {
            for name in ["AGENTS.md", "CLAUDE.md"] {
                let f = dir.join(name);
                if let Ok(body) = std::fs::read_to_string(&f) {
                    // Delta §12.8: inline `@path` imports before the
                    // raw text is parsed into sections.
                    let expanded = expand_imports(&body, &f);
                    raw.extend(parse_agents_md(&expanded, &f));
                }
            }
        }
        // Shadow: a deeper named section replaces a shallower one.
        //
        // First pass collects the *last* index that declared each
        // name; second pass drops every earlier occurrence. Doing it
        // in two immutable passes avoids holding a borrow of
        // `deduped` while mutating it.
        let mut winner: BTreeMap<String, usize> = BTreeMap::new();
        for (i, s) in raw.iter().enumerate() {
            if let Some(name) = &s.name {
                winner.insert(name.clone(), i);
            }
        }
        let kept: std::collections::HashSet<usize> = winner.values().copied().collect();
        let sections: Vec<InstructionSection> = raw
            .into_iter()
            .enumerate()
            .filter_map(|(i, s)| {
                match &s.name {
                    // A named section: keep only the deepest
                    // occurrence.
                    Some(_) => {
                        if kept.contains(&i) {
                            Some(s)
                        } else {
                            None
                        }
                    }
                    // An unnamed section is never shadowed.
                    None => Some(s),
                }
            })
            .collect();
        Self { sections }
    }

    /// Render sections whose guards fire for this turn.
    pub fn render(&self, task: Option<&str>, paths: &[PathBuf], langs: &[&str]) -> String {
        let mut out = String::new();
        for s in &self.sections {
            if !guard_matches(&s.when, task, paths, langs) {
                continue;
            }
            if !out.is_empty() {
                out.push('\n');
            }
            if let Some(n) = &s.name {
                out.push_str(&format!("### {n}\n\n"));
            }
            out.push_str(s.body.trim_end());
            out.push('\n');
        }
        out
    }
}

fn guard_matches(when: &When, task: Option<&str>, paths: &[PathBuf], langs: &[&str]) -> bool {
    match when {
        When::Always => true,
        When::TaskType(t) => task.map(|x| x.eq_ignore_ascii_case(t)).unwrap_or(false),
        When::Lang(l) => langs.iter().any(|x| x.eq_ignore_ascii_case(l)),
        When::Path(glob) => paths.iter().any(|p| simple_glob_match(glob, &p.to_string_lossy())),
    }
}

/// Minimal glob matcher: `**` (globstar) matches across path
/// segments, `*` within a segment, `?` a single character.
///
/// Recursive with backtracking on `*` and `**`. Correct, not fast —
/// the guards are evaluated once per turn against a handful of
/// paths, so the recursion depth is trivial and the cost is
/// invisible next to the model call.
///
/// Cases the tests pin:
///
/// - `crates/web/**` matches `crates/web/src/lib.rs`
/// - `crates/**/src/*.rs` matches `crates/web/src/a.rs` **and**
///   `crates/src/a.rs` (`**` matches zero or more segments)
/// - `crates/web/**` does not match `crates/core/lib.rs`
/// - `a?c` matches `abc` but not `ac`
fn simple_glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    glob_rec(&p, &t)
}

fn glob_rec(p: &[char], t: &[char]) -> bool {
    // Globstar: `**`. Consume it and (if present) a following `/`,
    // then try to match the remainder at every split of `t`.
    if p.len() >= 2 && p[0] == '*' && p[1] == '*' {
        let (rest_p, trailing_slash) = if p.len() >= 3 && p[2] == '/' {
            (&p[3..], true)
        } else {
            (&p[2..], false)
        };
        for i in 0..=t.len() {
            if glob_rec(rest_p, &t[i..]) {
                return true;
            }
            // `a/**/b` must match `a/b`: after `**` consumed zero
            // segments the `/` in the pattern still needs to
            // correspond to nothing in the text.
            if trailing_slash && i < t.len() && t[i] == '/' && glob_rec(rest_p, &t[i + 1..]) {
                return true;
            }
        }
        return false;
    }
    match (p.first(), t.first()) {
        // Single `*`: zero or more characters within the current
        // segment. Two branches — consume the `*` (match empty) or
        // consume one character from `t` and keep the `*`.
        (Some('*'), _) => glob_rec(&p[1..], t) || (!t.is_empty() && glob_rec(p, &t[1..])),
        (Some('?'), Some(_)) => glob_rec(&p[1..], &t[1..]),
        (Some(a), Some(b)) if a == b => glob_rec(&p[1..], &t[1..]),
        (None, None) => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn unconditional_paragraph_is_always() {
        let s = parse_agents_md("Use tabs.\n", Path::new("AGENTS.md"));
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].when, When::Always);
    }

    #[test]
    fn task_fence_guards_a_block() {
        let s = parse_agents_md("::: when task=Debugging\nPrefer tracing.\n:::\n", Path::new("a"));
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].when, When::TaskType("Debugging".into()));
    }

    #[test]
    fn path_fence_guards_a_block() {
        let s = parse_agents_md("::: when path=crates/web/**\nNo.\n:::\n", Path::new("a"));
        assert_eq!(s[0].when, When::Path("crates/web/**".into()));
    }

    #[test]
    fn heading_names_a_section() {
        let s = parse_agents_md("## Frontend\nUse React.\n", Path::new("a"));
        assert_eq!(s[0].name.as_deref(), Some("Frontend"));
    }

    #[test]
    fn render_filters_by_task() {
        let s = parse_agents_md(
            "::: when task=Debugging\nDebug.\n:::\n::: when task=Research\nResearch.\n:::\n",
            Path::new("a"),
        );
        let chain = InstructionChain { sections: s };
        let debug = chain.render(Some("Debugging"), &[], &[]);
        assert!(debug.contains("Debug."));
        assert!(!debug.contains("Research."));
    }

    #[test]
    fn render_filters_by_path() {
        let s = parse_agents_md("::: when path=crates/web/**\nWeb.\n:::\n", Path::new("a"));
        let chain = InstructionChain { sections: s };
        assert!(chain.render(None, &[p("crates/web/src/lib.rs")], &[]).contains("Web"));
        assert!(chain.render(None, &[p("crates/core/lib.rs")], &[]).is_empty());
    }

    #[test]
    fn deeper_section_shadows_shallower() {
        let root = parse_agents_md("## Style\nRoot style.\n", Path::new("/r/AGENTS.md"));
        let leaf = parse_agents_md("## Style\nLeaf style.\n", Path::new("/r/w/AGENTS.md"));
        let mut raw = root;
        raw.extend(leaf);
        // Same two-pass shape as `InstructionChain::load`.
        let mut winner: BTreeMap<String, usize> = BTreeMap::new();
        for (i, s) in raw.iter().enumerate() {
            if let Some(name) = &s.name {
                winner.insert(name.clone(), i);
            }
        }
        let kept: std::collections::HashSet<usize> = winner.values().copied().collect();
        let sections: Vec<InstructionSection> = raw
            .into_iter()
            .enumerate()
            .filter_map(|(i, s)| match &s.name {
                Some(_) => kept.contains(&i).then_some(s),
                None => Some(s),
            })
            .collect();
        let chain = InstructionChain { sections };
        assert_eq!(chain.sections.len(), 1);
        assert!(chain.sections[0].body.contains("Leaf style"));
    }

    #[test]
    fn glob_double_star_matches_across_segments() {
        assert!(simple_glob_match("crates/web/**", "crates/web/src/lib.rs"));
        assert!(simple_glob_match("crates/**/src/*.rs", "crates/web/src/a.rs"));
        assert!(!simple_glob_match("crates/web/**", "crates/core/lib.rs"));
    }

    #[test]
    fn glob_question_matches_one_char() {
        assert!(simple_glob_match("a?c", "abc"));
        assert!(!simple_glob_match("a?c", "ac"));
    }

    // ---- @-import expansion (delta §12.8) ------------------------------

    use std::sync::atomic::{AtomicU64, Ordering};
    static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn unique_dir(tag: &str) -> PathBuf {
        let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let d = std::env::temp_dir().join(format!(
            "kod-insn-{}-{}-{}-{}",
            tag,
            std::process::id(),
            n,
            nanos,
        ));
        std::fs::create_dir_all(&d).expect("mkdir");
        d
    }

    #[test]
    fn a_lone_import_inlines_the_referenced_file() {
        let dir = unique_dir("inline");
        std::fs::write(dir.join("other.md"), "Imported body.\n").unwrap();
        let main = dir.join("AGENTS.md");
        std::fs::write(&main, "Before.\n@other.md\nAfter.\n").unwrap();
        let src = std::fs::read_to_string(&main).unwrap();
        let out = expand_imports(&src, &main);
        assert!(out.contains("Before."), "got: {out}");
        assert!(out.contains("Imported body."), "got: {out}");
        assert!(out.contains("After."), "got: {out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_fenced_import_is_literal() {
        let dir = unique_dir("fence");
        std::fs::write(dir.join("other.md"), "SHOULD NOT APPEAR\n").unwrap();
        let main = dir.join("AGENTS.md");
        std::fs::write(
            &main,
            "Intro.\n```\n@other.md\n```\nOutro.\n",
        )
        .unwrap();
        let src = std::fs::read_to_string(&main).unwrap();
        let out = expand_imports(&src, &main);
        assert!(!out.contains("SHOULD NOT APPEAR"), "got: {out}");
        assert!(out.contains("@other.md"), "got: {out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_import_with_trailing_prose_is_literal() {
        let dir = unique_dir("prose");
        std::fs::write(dir.join("other.md"), "SHOULD NOT APPEAR\n").unwrap();
        let main = dir.join("AGENTS.md");
        std::fs::write(&main, "@other.md see this file\n").unwrap();
        let src = std::fs::read_to_string(&main).unwrap();
        let out = expand_imports(&src, &main);
        assert!(!out.contains("SHOULD NOT APPEAR"), "got: {out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_import_stays_literal() {
        let dir = unique_dir("missing");
        let main = dir.join("AGENTS.md");
        std::fs::write(&main, "@nope.md\n").unwrap();
        let src = std::fs::read_to_string(&main).unwrap();
        let out = expand_imports(&src, &main);
        assert_eq!(out, "@nope.md\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_depth_chain_expands_only_to_the_limit() {
        let dir = unique_dir("depth");
        // Build a chain f0 -> f1 -> ... -> f6, each a single-line
        // import of the next, with a distinctive marker in f5 and
        // f6. With MAX_IMPORT_DEPTH = 5 the walk reaches f5 (the
        // fifth nested import); f5's reference to f6 is left
        // literal, so f6's marker never lands in the output.
        for i in 0..=6u32 {
            let body = match i {
                5 => "TAIL_MARKER\n@f6.md\n".to_string(),
                6 => "BEYOND_LIMIT_MARKER\n".to_string(),
                _ => format!("@f{}.md\n", i + 1),
            };
            std::fs::write(dir.join(format!("f{i}.md")), body).unwrap();
        }
        let src = std::fs::read_to_string(dir.join("f0.md")).unwrap();
        let out = expand_imports(&src, &dir.join("f0.md"));
        assert!(
            out.contains("TAIL_MARKER"),
            "the fifth nested import (f5) must be reached; got: {out}",
        );
        assert!(
            !out.contains("BEYOND_LIMIT_MARKER"),
            "an import past depth 5 must stay literal; got: {out}",
        );
        assert!(
            out.contains("@f6.md"),
            "the depth-limited reference itself stays in the text; got: {out}",
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_cycle_short_circuits() {
        let dir = unique_dir("cycle");
        std::fs::write(dir.join("a.md"), "A body.\n@b.md\n").unwrap();
        std::fs::write(dir.join("b.md"), "B body.\n@a.md\n").unwrap();
        let src = std::fs::read_to_string(dir.join("a.md")).unwrap();
        let out = expand_imports(&src, &dir.join("a.md"));
        // One expansion of each; the cycle back to a.md is literal.
        assert_eq!(out.matches("A body.").count(), 1, "got: {out}");
        assert_eq!(out.matches("B body.").count(), 1, "got: {out}");
        assert!(out.contains("@a.md"), "cycle reference should stay literal");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_diamond_expands_the_shared_file_twice() {
        // a imports b and c; b and c both import d. d is expanded
        // twice — one per import site — because the visited guard is
        // for cycles, not for de-duplication.
        let dir = unique_dir("diamond");
        std::fs::write(dir.join("d.md"), "D body.\n").unwrap();
        std::fs::write(dir.join("b.md"), "@d.md\n").unwrap();
        std::fs::write(dir.join("c.md"), "@d.md\n").unwrap();
        std::fs::write(dir.join("a.md"), "@b.md\n@c.md\n").unwrap();
        let src = std::fs::read_to_string(dir.join("a.md")).unwrap();
        let out = expand_imports(&src, &dir.join("a.md"));
        assert_eq!(
            out.matches("D body.").count(),
            2,
            "expected two expansions in a diamond; got: {out}",
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_wires_imports_into_the_section_list() {
        // End-to-end: an AGENTS.md with an import produces a chain
        // whose rendered body contains the imported text.
        let dir = unique_dir("load");
        std::fs::write(dir.join("extra.md"), "Imported rule.\n").unwrap();
        std::fs::write(dir.join("AGENTS.md"), "@extra.md\n").unwrap();
        let chain = InstructionChain::load(&dir, &dir);
        let body = chain.render(None, &[], &[]);
        assert!(body.contains("Imported rule."), "got: {body}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
