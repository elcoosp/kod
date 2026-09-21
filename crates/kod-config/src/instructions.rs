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
                    raw.extend(parse_agents_md(&body, &f));
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

/// Minimal glob matcher: `**` matches across path segments, `*`
/// within a segment, `?` one char.
fn simple_glob_match(pattern: &str, text: &str) -> bool {
    let parts: Vec<&str> = pattern.split("**").collect();
    if parts.len() == 1 {
        return segment_match(pattern, text);
    }
    let mut start = 0usize;
    for (i, part) in parts.iter().enumerate() {
        let part = part.trim_matches('/');
        if part.is_empty() {
            continue;
        }
        match text[start..].find(part) {
            Some(pos) => start += pos + part.len(),
            None => return false,
        }
        if i == parts.len() - 1 {
            // Nothing after the last `**` needs matching.
        }
    }
    true
}

fn segment_match(pattern: &str, text: &str) -> bool {
    // Recursive wildcard match with backtracking on `*`.
    fn go(p: &[char], t: &[char]) -> bool {
        match (p.first(), t.first()) {
            (Some('*'), _) => {
                // `*` matches zero or more characters.
                go(&p[1..], t) || (!t.is_empty() && go(p, &t[1..]))
            }
            (Some('?'), Some(_)) => go(&p[1..], &t[1..]),
            (Some(a), Some(b)) if a == b => go(&p[1..], &t[1..]),
            (None, None) => true,
            _ => false,
        }
    }
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    go(&p, &t)
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
        let mut by_name: BTreeMap<String, usize> = BTreeMap::new();
        let mut dedup: Vec<Option<InstructionSection>> = raw.into_iter().map(Some).collect();
        for (i, s) in dedup.iter().enumerate() {
            if let Some(s) = s
                && let Some(name) = &s.name
            {
                if let Some(&prev) = by_name.get(name) {
                    dedup[prev] = None;
                }
                by_name.insert(name.clone(), i);
            }
        }
        let chain = InstructionChain {
            sections: dedup.into_iter().flatten().collect(),
        };
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
}
