//! One rule for "what is a skill file".
//!
//! Three places used to answer this question independently
//! (`SkillLoader::scan_directory`, `doctor::collect_md_files`, and
//! `CapabilityRegistry::discover`) and they disagreed: the loader
//! walked one level, the doctor walked the whole tree, and the
//! registry looked only at the top. A curated collection
//! (Claude / Codex / Cursor style) therefore both *lost* real skills
//! — a nested `SKILL.md` none of the three reached — and *gained*
//! false ones: the doctor parsed every `references/*.md` and warned
//! about missing front matter.
//!
//! The rule, matching the convention those collections use:
//!
//! * A file named `SKILL.md`, at any depth, is a skill entrypoint.
//!   The depth matters: `.codex/skills/.system/imagegen/SKILL.md` is
//!   a real skill two directories below the root.
//! * A `.md` file directly in the skills directory is a flat skill
//!   (kod's original shape; kept so a single-file skill still loads).
//! * Every other `.md` — `references/*.md`, `examples/*.md`,
//!   `README.md`, `CLAUDE.md` — is a resource a skill points a reader
//!   at, not a skill.
//!
//! Symlinked directories are not followed. A link cycle would hang the
//! walk, and the convention is a real tree.

use std::path::{Path, PathBuf};

/// The filename that marks a directory as a skill package.
pub const SKILL_ENTRYPOINT: &str = "SKILL.md";

/// Every skill file under `root`, in deterministic (sorted) order.
///
/// A missing or unreadable `root` yields an empty vec rather than an
/// error: a caller that probes several candidate directories should
/// not have to special-case the absent ones.
pub fn discover_skill_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if !root.is_dir() {
        return out;
    }
    walk(root, true, &mut out);
    out.sort();
    out
}

/// Whether `path` is a skill file directly under `root` (flat `.md`)
/// or a `SKILL.md` anywhere. Exposed for callers that already have a
/// path and want the rule without re-walking — the watcher, for one.
pub fn is_skill_file(root: &Path, path: &Path) -> bool {
    if path.file_name().and_then(|n| n.to_str()) == Some(SKILL_ENTRYPOINT) {
        return true;
    }
    let is_top_level = path.parent() == Some(root);
    is_top_level && path.extension().and_then(|s| s.to_str()) == Some("md")
}

fn walk(dir: &Path, is_root: bool, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        // `file_type()` does not follow symlinks, so a symlinked
        // directory reports neither `is_dir` nor `is_file` and falls
        // through untouched.
        let Ok(ft) = entry.file_type() else { continue };
        let path = entry.path();
        if ft.is_dir() {
            walk(&path, false, out);
        } else if ft.is_file() {
            // A `SKILL.md` anywhere is a skill; a flat `.md` at the
            // root is one too (kod's original single-file shape).
            // Every other `.md` is a resource a skill points at.
            let is_entrypoint =
                path.file_name().and_then(|n| n.to_str()) == Some(SKILL_ENTRYPOINT);
            let is_flat_skill =
                is_root && path.extension().and_then(|s| s.to_str()) == Some("md");
            if is_entrypoint || is_flat_skill {
                out.push(path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn touch(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn missing_root_is_empty() {
        assert!(discover_skill_files(Path::new("/no/such/dir/kod-xyz")).is_empty());
    }

    #[test]
    fn a_flat_top_level_md_is_a_skill() {
        let t = TempDir::new().unwrap();
        touch(&t.path().join("flat.md"), "x");
        touch(&t.path().join("notes.txt"), "x");
        let got = discover_skill_files(t.path());
        assert_eq!(got.len(), 1, "got {got:?}");
        assert!(got[0].ends_with("flat.md"));
    }

    #[test]
    fn a_nested_skill_md_is_a_skill() {
        let t = TempDir::new().unwrap();
        touch(&t.path().join("pkg").join("SKILL.md"), "x");
        let got = discover_skill_files(t.path());
        assert_eq!(got.len(), 1, "got {got:?}");
        assert!(got[0].ends_with("SKILL.md"));
    }

    #[test]
    fn a_deeply_nested_skill_md_is_a_skill() {
        // The `.codex/skills/.system/<pkg>/SKILL.md` shape.
        let t = TempDir::new().unwrap();
        touch(
            &t.path().join(".system").join("imagegen").join("SKILL.md"),
            "x",
        );
        let got = discover_skill_files(t.path());
        assert_eq!(got.len(), 1, "got {got:?}");
    }

    #[test]
    fn reference_files_are_not_skills() {
        let t = TempDir::new().unwrap();
        touch(&t.path().join("pkg").join("SKILL.md"), "x");
        touch(
            &t.path()
                .join("pkg")
                .join("references")
                .join("sample-prompts.md"),
            "x",
        );
        touch(&t.path().join("pkg").join("examples").join("demo.md"), "x");
        touch(&t.path().join("pkg").join("README.md"), "x");
        let got = discover_skill_files(t.path());
        assert_eq!(got.len(), 1, "only the SKILL.md is a skill: {got:?}");
        assert!(got[0].ends_with("SKILL.md"));
    }

    #[test]
    fn a_markdown_file_inside_a_package_that_is_not_skill_md_is_not_a_skill() {
        let t = TempDir::new().unwrap();
        touch(&t.path().join("pkg").join("guide.md"), "x");
        assert!(discover_skill_files(t.path()).is_empty());
    }

    #[test]
    fn symlinked_directories_are_not_followed() {
        #[cfg(unix)]
        {
            let t = TempDir::new().unwrap();
            let real = t.path().join("real");
            touch(&real.join("SKILL.md"), "x");
            let link = t.path().join("link");
            std::os::unix::fs::symlink(&real, &link).unwrap();
            // `real` is still reached by the normal walk; the link is
            // not followed, so the skill is found exactly once.
            let got = discover_skill_files(t.path());
            assert_eq!(got.len(), 1, "got {got:?}");
        }
    }

    #[test]
    fn result_is_sorted() {
        let t = TempDir::new().unwrap();
        touch(&t.path().join("b").join("SKILL.md"), "x");
        touch(&t.path().join("a").join("SKILL.md"), "x");
        let got = discover_skill_files(t.path());
        let mut sorted = got.clone();
        sorted.sort();
        assert_eq!(got, sorted);
    }
}
