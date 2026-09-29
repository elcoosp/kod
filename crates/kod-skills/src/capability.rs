//! Capability discovery registry (borrow from oh-my-pi, delta §14.2).
//!
//! # The problem
//!
//! A skill, MCP server, command, or rule lives in a directory. kod
//! reads four: `~/.kod/skills`, `~/.agents/skills`, `<cwd>/.kod/skills`,
//! `<cwd>/.agents/skills`. Other harnesses use their own: Claude reads
//! `.claude/`, Cursor reads `.cursor/`, Gemini reads `.gemini/`. A user
//! who already curates a `.claude/skills` directory should be able to
//! use it without copying.
//!
//! # The registry
//!
//! A [`CapabilityRegistry`] collects discovery *sources*. Each source
//! names a directory, a format, and a priority *band*. Sources load
//! in parallel; a duplicate key loses to the higher-priority source
//! (first party beats a foreign directory, project beats global).
//!
//! `suppress()` differs from `filter()`: a suppressed item is still
//! *recorded* — it claims its dedup key — but the caller does not see
//! it. That is what lets a user disable a project-level skill without
//! having the same-named user-level skill appear in its place.
//!
//! # What this is NOT
//!
//! * Not a loader. It walks directories and returns descriptors; the
//!   skill / MCP / command parser turns a path into a value.
//! * Not a config reader. A caller builds the sources from config
//!   and hands them in.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Priority bands, highest first.
///
/// A source with a higher band wins a duplicate key. The ordering is
/// what makes "a project skill overrides a global skill of the same
/// name" work without the caller sorting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Band {
    /// A user-curated directory (Claude, Cursor, Gemini, …). Lowest
    /// priority: a kod-native source always wins, and a project
    /// override still wins over a foreign global.
    Foreign = 0,
    /// A built-in or shipped capability.
    Builtin = 1,
    /// A global config directory (`~/.kod`, `~/.agents`).
    Global = 2,
    /// A project-local directory (`<cwd>/.kod`, `<cwd>/.agents`).
    Project = 3,
}

/// A directory a registry reads capabilities from.
#[derive(Debug, Clone)]
pub struct DiscoverySource {
    /// The directory.
    pub dir: PathBuf,
    /// Free-form label ("kod-global", "claude-project") used in
    /// logs and in the `_source` field a caller renders.
    pub label: String,
    pub band: Band,
}

impl DiscoverySource {
    pub fn new(dir: impl Into<PathBuf>, label: impl Into<String>, band: Band) -> Self {
        Self {
            dir: dir.into(),
            label: label.into(),
            band,
        }
    }
}

/// One discovered capability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovered {
    /// The dedup key: a skill name, an MCP server name, a command
    /// name. Two sources that produce the same key conflict.
    pub key: String,
    /// The path the item was discovered at.
    pub path: PathBuf,
    /// The label of the source that produced it.
    pub source: String,
    pub band: Band,
}

/// The registry.
///
/// Not thread-safe internally; a caller drives it from one task.
/// Load is `async` because a future extension walks directory trees
/// and reads front matter — the shape is right today even though the
/// current implementation is a shallow `read_dir`.
#[derive(Debug, Default)]
pub struct CapabilityRegistry {
    sources: Vec<DiscoverySource>,
}

impl CapabilityRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register one source.
    pub fn add(&mut self, source: DiscoverySource) -> &mut Self {
        self.sources.push(source);
        self
    }

    /// The built-in priority-ordered default: project kod, project
    /// agents, global kod, global agents, then the common foreign
    /// directories at the matching scope.
    ///
    /// `home` and `cwd` are taken as arguments so a test can drive
    /// the layout without touching the real filesystem.
    pub fn default_sources(home: &Path, cwd: &Path) -> Vec<DiscoverySource> {
        vec![
            DiscoverySource::new(
                cwd.join(".kod").join("skills"),
                "kod-project",
                Band::Project,
            ),
            DiscoverySource::new(
                cwd.join(".agents").join("skills"),
                "agents-project",
                Band::Project,
            ),
            DiscoverySource::new(
                cwd.join(".claude").join("skills"),
                "claude-project",
                Band::Foreign,
            ),
            DiscoverySource::new(
                cwd.join(".cursor").join("skills"),
                "cursor-project",
                Band::Foreign,
            ),
            DiscoverySource::new(home.join(".kod").join("skills"), "kod-global", Band::Global),
            DiscoverySource::new(
                home.join(".agents").join("skills"),
                "agents-global",
                Band::Global,
            ),
            DiscoverySource::new(
                home.join(".claude").join("skills"),
                "claude-global",
                Band::Foreign,
            ),
            DiscoverySource::new(
                home.join(".cursor").join("skills"),
                "cursor-global",
                Band::Foreign,
            ),
            DiscoverySource::new(
                home.join(".gemini").join("skills"),
                "gemini-global",
                Band::Foreign,
            ),
        ]
    }

    /// Walk every source and return the *winning* item for each key.
    ///
    /// A duplicate key keeps the higher band; ties keep the earlier
    /// source (registration order). A `suppressed` key is dropped
    /// from the result *after* claiming its dedup slot — see the
    /// module docs.
    ///
    /// `extension` filters the filenames: `.md` for a skill,
    /// `.toml` for a config. `None` accepts every file.
    pub fn discover(&self, extension: Option<&str>) -> Vec<Discovered> {
        // Key -> winning candidate.
        let mut winners: BTreeMap<String, Discovered> = BTreeMap::new();
        for src in &self.sources {
            let Ok(entries) = std::fs::read_dir(&src.dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                if let Some(ext) = extension
                    && path.extension().and_then(|e| e.to_str()) != Some(ext)
                {
                    continue;
                }
                let Some(key) = path.file_stem().and_then(|s| s.to_str()).map(String::from) else {
                    continue;
                };
                let candidate = Discovered {
                    key: key.clone(),
                    path,
                    source: src.label.clone(),
                    band: src.band,
                };
                match winners.get(&key) {
                    Some(existing) if existing.band >= candidate.band => {}
                    _ => {
                        winners.insert(key, candidate);
                    }
                }
            }
        }
        winners.into_values().collect()
    }

    /// Discover and drop the items whose keys are in `suppressed`.
    ///
    /// The two functions differ on purpose: `suppress` removes an
    /// item *after* it has claimed its dedup slot (so a lower-band
    /// duplicate does not take its place), while `filter` is a plain
    /// predicate the caller can apply to the *result*. `suppress` is
    /// the correct primitive for "the user turned this off."
    pub fn discover_with_suppression(
        &self,
        extension: Option<&str>,
        suppressed: &std::collections::HashSet<String>,
    ) -> Vec<Discovered> {
        self.discover(extension)
            .into_iter()
            .filter(|d| !suppressed.contains(&d.key))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmpdir() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "kod-cap-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        ));
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn write(dir: &Path, name: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join(name), "x").unwrap();
    }

    #[test]
    fn an_empty_registry_discovers_nothing() {
        let r = CapabilityRegistry::new();
        assert!(r.discover(None).is_empty());
    }

    #[test]
    fn discover_reads_a_source() {
        let dir = tmpdir();
        write(&dir, "alpha.md");
        write(&dir, "beta.md");
        let mut r = CapabilityRegistry::new();
        r.add(DiscoverySource::new(&dir, "test", Band::Project));
        let found = r.discover(Some("md"));
        let keys: Vec<&str> = found.iter().map(|d| d.key.as_str()).collect();
        assert_eq!(keys, vec!["alpha", "beta"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_extension_filter_applies() {
        let dir = tmpdir();
        write(&dir, "alpha.md");
        write(&dir, "beta.toml");
        let mut r = CapabilityRegistry::new();
        r.add(DiscoverySource::new(&dir, "test", Band::Project));
        assert_eq!(r.discover(Some("md")).len(), 1);
        assert_eq!(r.discover(Some("toml")).len(), 1);
        assert_eq!(r.discover(None).len(), 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_higher_band_wins_a_duplicate() {
        let project = tmpdir();
        let global = tmpdir();
        write(&project, "shared.md");
        write(&global, "shared.md");
        let mut r = CapabilityRegistry::new();
        r.add(DiscoverySource::new(&global, "global", Band::Global));
        r.add(DiscoverySource::new(&project, "project", Band::Project));
        let found = r.discover(Some("md"));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].source, "project");
        let _ = fs::remove_dir_all(&project);
        let _ = fs::remove_dir_all(&global);
    }

    #[test]
    fn the_lower_band_does_not_replace_a_winner_registered_first() {
        // Same band, registered earlier wins.
        let a = tmpdir();
        let b = tmpdir();
        write(&a, "same.md");
        write(&b, "same.md");
        let mut r = CapabilityRegistry::new();
        r.add(DiscoverySource::new(&a, "first", Band::Project));
        r.add(DiscoverySource::new(&b, "second", Band::Project));
        let found = r.discover(Some("md"));
        assert_eq!(found[0].source, "first");
        let _ = fs::remove_dir_all(&a);
        let _ = fs::remove_dir_all(&b);
    }

    #[test]
    fn suppress_drops_an_item_after_it_claims_its_key() {
        let project = tmpdir();
        let global = tmpdir();
        write(&project, "keep.md");
        write(&project, "off.md");
        write(&global, "off.md"); // a lower-band duplicate
        let mut r = CapabilityRegistry::new();
        r.add(DiscoverySource::new(&global, "global", Band::Global));
        r.add(DiscoverySource::new(&project, "project", Band::Project));
        let mut suppressed = std::collections::HashSet::new();
        suppressed.insert("off".to_string());
        let found = r.discover_with_suppression(Some("md"), &suppressed);
        // `keep` survives; `off` is gone entirely (the lower-band
        // duplicate did not take its place).
        let keys: Vec<&str> = found.iter().map(|d| d.key.as_str()).collect();
        assert_eq!(keys, vec!["keep"]);
        let _ = fs::remove_dir_all(&project);
        let _ = fs::remove_dir_all(&global);
    }

    #[test]
    fn an_absent_source_directory_is_skipped() {
        let missing = PathBuf::from("/nonexistent/kod-cap-test");
        let mut r = CapabilityRegistry::new();
        r.add(DiscoverySource::new(&missing, "missing", Band::Project));
        assert!(r.discover(None).is_empty());
    }

    #[test]
    fn default_sources_lists_project_before_global_before_foreign() {
        let home = PathBuf::from("/home/u");
        let cwd = PathBuf::from("/repo");
        let sources = CapabilityRegistry::default_sources(&home, &cwd);
        // First is a project source; last is a foreign global.
        assert_eq!(sources[0].band, Band::Project);
        assert_eq!(sources.last().unwrap().band, Band::Foreign);
        // A foreign project directory sits between the kod project
        // and the kod global sources.
        let claude_project = sources
            .iter()
            .find(|s| s.label == "claude-project")
            .expect("claude-project source");
        assert_eq!(claude_project.band, Band::Foreign);
    }

    #[test]
    fn default_sources_cover_the_named_foreign_directories() {
        let home = PathBuf::from("/home/u");
        let cwd = PathBuf::from("/repo");
        let labels: Vec<String> = CapabilityRegistry::default_sources(&home, &cwd)
            .into_iter()
            .map(|s| s.label)
            .collect();
        for want in [
            "claude-project",
            "cursor-project",
            "claude-global",
            "gemini-global",
        ] {
            assert!(labels.iter().any(|l| l == want), "missing {want}");
        }
    }
}
