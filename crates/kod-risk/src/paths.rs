//! Lexical path classification.
//!
//! No filesystem access: every answer comes from the string alone.
//! That is deliberate — a classifier that `stat`s a path can be
//! raced, and the command it is classifying may create or remove the
//! path it points at. A purely lexical classifier is deterministic
//! and cannot be tricked by timing.

use std::path::{Component, Path, PathBuf};

/// How dangerous an operation on a path is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PathDanger {
    Safe,
    Low,
    Confirm,
    Catastrophic,
}

/// The directories a classification is relative to.
#[derive(Debug, Clone)]
pub struct RiskContext {
    pub working_dir: PathBuf,
    pub home_dir: PathBuf,
    pub scratch_dir: PathBuf,
}

impl RiskContext {
    /// Build from the process environment. `HOME` is required; a
    /// missing one falls back to `/` so the classifier never panics.
    pub fn from_env() -> Self {
        let home_dir = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/"));
        let working_dir = std::env::current_dir().unwrap_or_else(|_| home_dir.clone());
        let scratch_dir = std::env::temp_dir();
        Self {
            working_dir,
            home_dir,
            scratch_dir,
        }
    }
}

/// Credential stores: protected **recursively**. A path *inside* one
/// of these is as dangerous as the store itself — `~/.ssh/id_rsa` is
/// a key, `~/.ssh/known_hosts` is not, but telling them apart by name
/// is a losing game and the cost of over-protecting is one prompt.
const PROTECTED_DIRS: &[&str] = &[".ssh", ".gnupg", ".aws", ".kube", ".docker"];

/// System directories dangerous to write into, matched **exactly**.
/// `/usr` is not here: `/usr/local/bin` is a legitimate install
/// target. `/etc` is here and also in the recursive list below.
const SYSTEM_EXACT: &[&str] = &[
    "/", "/etc", "/usr", "/var", "/bin", "/sbin", "/boot", "/sys", "/proc",
    // The parents of every user home. Exact-match only: deleting
    // `/Users` removes every account's home, but `/Users/other/proj`
    // is an ordinary path and must not be flagged.
    "/Users", "/home",
];

/// System directories dangerous to write into **recursively**.
const SYSTEM_RECURSIVE: &[&str] = &["/etc", "/boot", "/sys", "/proc"];

/// Expand `$HOME` and `${HOME}` and a leading `~` **lexically**.
///
/// No other variable is expanded: `$HOME_BACKUP` must not become
/// `/Users/x_BACKUP`, and `$EVIL` must not become anything — an
/// unresolved variable is a path the classifier cannot reason about,
/// which is itself a reason to escalate (the caller decides).
///
/// Returns the expanded string and whether any variable was left
/// unresolved.
pub fn expand_lexical(raw: &str, home: &Path) -> (String, bool) {
    let mut out = String::with_capacity(raw.len());
    let mut unresolved = false;
    let home_str = home.to_string_lossy();

    // A leading `~` (alone or before `/`) is the home directory.
    let rest = if let Some(r) = raw.strip_prefix("~/") {
        out.push_str(&home_str);
        out.push('/');
        r
    } else if raw == "~" {
        out.push_str(&home_str);
        ""
    } else {
        raw
    };

    let bytes = rest.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            // `${NAME}`
            if let Some(end) = rest[i + 2..].find('}') {
                let name = &rest[i + 2..i + 2 + end];
                if name == "HOME" {
                    out.push_str(&home_str);
                } else {
                    unresolved = true;
                }
                i += 2 + end + 1;
                continue;
            }
            unresolved = true;
            i += 1;
            continue;
        }
        if bytes[i] == b'$' && i + 1 < bytes.len() {
            // `$NAME` — read the identifier.
            let start = i + 1;
            let mut end = start;
            while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
                end += 1;
            }
            let name = &rest[start..end];
            if name == "HOME" {
                out.push_str(&home_str);
            } else if !name.is_empty() {
                unresolved = true;
            } else {
                // A bare `$` with nothing after: literal.
                out.push('$');
            }
            i = end;
            continue;
        }
        // Copy the whole codepoint, not the byte: `bytes[i] as char`
        // transcoded UTF-8 to Latin-1, so `strip_prefix(home)` failed
        // for a non-ASCII home dir and credential paths classified Low.
        if bytes[i].is_ascii() {
            out.push(bytes[i] as char);
            i += 1;
        } else {
            let ch = rest[i..].chars().next().unwrap_or('\u{FFFD}');
            out.push(ch);
            i += ch.len_utf8();
        }
    }

    (out, unresolved)
}

/// Lexically normalize a path: resolve `.` and `..` components without
/// touching the filesystem. `~/../..` normalizes to `/`, which is the
/// point — a path that walks out of a protected directory is as
/// dangerous as one that names it.
pub fn normalize(path: &str) -> PathBuf {
    let p = Path::new(path);
    let mut out: Vec<Component> = Vec::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                // Do not pop past root.
                if matches!(out.last(), Some(Component::Normal(_))) {
                    out.pop();
                }
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    let mut buf = PathBuf::new();
    for c in out {
        buf.push(c);
    }
    if buf.as_os_str().is_empty() {
        buf.push("/");
    }
    buf
}

/// Whether `path` is one of the protected credential stores or inside
/// one.
fn in_credential_store(path: &Path, home: &Path) -> bool {
    let Ok(rel) = path.strip_prefix(home) else {
        return false;
    };
    rel.components().any(|c| {
        if let Component::Normal(name) = c {
            PROTECTED_DIRS.iter().any(|d| name == *d)
        } else {
            false
        }
    })
}

/// Classify a single path string.
///
/// The `unresolved` flag from [`expand_lexical`] is returned so the
/// caller can escalate on it; this function treats an unresolved
/// variable as `Confirm`, because a path it cannot see is a path it
/// cannot clear.
pub fn classify(raw: &str, ctx: &RiskContext) -> (PathDanger, String) {
    let (expanded, unresolved) = expand_lexical(raw, &ctx.home_dir);
    let normalized = normalize(&expanded);
    let s = normalized.to_string_lossy();

    // A glob whose parent is a protected directory. `~/.ssh/*` is
    // "every key"; `*` alone is "everything in cwd", which is Low.
    if raw.contains('*') || raw.contains('?') {
        let parent = normalized.parent().unwrap_or(Path::new("/"));
        if in_credential_store(parent, &ctx.home_dir)
            || SYSTEM_RECURSIVE.iter().any(|d| parent.starts_with(d))
        {
            return (
                PathDanger::Catastrophic,
                format!("glob over a protected directory: {raw}"),
            );
        }
    }

    // Device nodes: writing to a raw disk device destroys the
    // filesystem on it.
    if s.starts_with("/dev/") && s != "/dev/null" && s != "/dev/stdout" && s != "/dev/stderr" {
        return (PathDanger::Catastrophic, format!("raw device node: {raw}"));
    }

    // SEC-A-10: `.git` metadata is not a writable surface for shell
    // commands. `write_file`/`patch_file` gate it at the tool layer,
    // but a shell redirect into `.git/hooks/pre-commit` was
    // classified Safe (inside working_dir) — a hook planted there
    // runs on the next `git commit`, with full env.
    if normalized
        .components()
        .any(|c| c.as_os_str() == std::ffi::OsStr::new(".git"))
    {
        return (
            PathDanger::Catastrophic,
            format!(".git metadata is not writable: {raw}"),
        );
    }

    // Credential stores, recursive.
    if in_credential_store(&normalized, &ctx.home_dir) {
        return (PathDanger::Catastrophic, format!("credential store: {raw}"));
    }

    // The home directory itself.
    if normalized == ctx.home_dir {
        return (
            PathDanger::Catastrophic,
            format!("the home directory itself: {raw}"),
        );
    }

    // System directories: exact match is catastrophic, anything below
    // the recursive set is too.
    for d in SYSTEM_EXACT {
        if s == *d {
            return (PathDanger::Catastrophic, format!("system root: {raw}"));
        }
    }
    for d in SYSTEM_RECURSIVE {
        if s.starts_with(&format!("{d}/")) {
            return (
                PathDanger::Catastrophic,
                format!("inside a system directory: {raw}"),
            );
        }
    }

    // `/Users` and `/home` are deliberately *not* recursive: every
    // real user home is below them, and marking the parent catastrophic
    // would flag every read of `/home/user/project`.

    // SEC-A-07: the working/scratch directory *itself* (not a path
    // under it) is a whole-tree deletion — `rm -rf <wd>` must not
    // classify Safe just because it equals the prefix. Check equality
    // before the prefix shortcut.
    if normalized == ctx.working_dir || normalized == ctx.scratch_dir {
        return (
            PathDanger::Confirm,
            format!(
                "the {} directory itself: {raw}",
                if normalized == ctx.working_dir {
                    "working"
                } else {
                    "scratch"
                }
            ),
        );
    }
    // SEC-A-06: POSIX expands `~name`, `~+`, `~-`; the lexical
    // expander only handles `~` and `~/`. An unexpanded tilde that is
    // not the bare forms is a path we cannot resolve — escalate per
    // the module contract ("never refuses; it escalates").
    if raw.starts_with('~') && raw != "~" && !raw.starts_with("~/") {
        return (
            PathDanger::Confirm,
            format!("unresolved tilde expansion: {raw}"),
        );
    }
    // Inside the scratch dir or the working dir: safe.
    if normalized.starts_with(&ctx.scratch_dir) || normalized.starts_with(&ctx.working_dir) {
        return (PathDanger::Safe, String::new());
    }

    if unresolved {
        return (
            PathDanger::Confirm,
            format!("unresolved variable in path: {raw}"),
        );
    }

    (PathDanger::Low, String::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> RiskContext {
        RiskContext {
            working_dir: PathBuf::from("/work/proj"),
            home_dir: PathBuf::from("/Users/dev"),
            scratch_dir: PathBuf::from("/tmp"),
        }
    }

    #[test]
    fn ssh_key_is_catastrophic() {
        let (d, _) = classify("/Users/dev/.ssh/id_rsa", &ctx());
        assert_eq!(d, PathDanger::Catastrophic);
    }

    #[test]
    fn tilde_expands_to_home() {
        let (expanded, unresolved) = expand_lexical("~/notes.txt", Path::new("/Users/dev"));
        assert_eq!(expanded, "/Users/dev/notes.txt");
        assert!(!unresolved);
    }

    #[test]
    fn home_variable_expands_but_others_do_not() {
        let (expanded, unresolved) = expand_lexical("$HOME/x", Path::new("/Users/dev"));
        assert_eq!(expanded, "/Users/dev/x");
        assert!(!unresolved);

        let (_, unresolved) = expand_lexical("$BACKUP/x", Path::new("/Users/dev"));
        assert!(unresolved, "a non-HOME variable must be flagged");
    }

    #[test]
    fn home_backup_lookalike_does_not_expand() {
        // The classic trick: `$HOME_BACKUP` must not become
        // `/Users/dev_BACKUP`.
        let (expanded, unresolved) = expand_lexical("$HOME_BACKUP/x", Path::new("/Users/dev"));
        assert!(!expanded.contains("/Users/dev_BACKUP"));
        assert!(unresolved);
    }

    #[test]
    fn parent_traversal_normalizes() {
        assert_eq!(normalize("/a/b/../c"), PathBuf::from("/a/c"));
        assert_eq!(normalize("/../.."), PathBuf::from("/"));
        // `normalize` runs *after* tilde expansion in every caller,
        // so an unexpanded `~` is a plain component here and the
        // traversal pops it like any other — the result is `/`. The
        // tilde's meaning is `expand_lexical`'s job, tested above.
        assert_eq!(normalize("~/../.."), PathBuf::from("/"));
    }

    #[cfg(unix)]
    #[test]
    fn etc_exact_is_catastrophic_and_below_it_too() {
        assert_eq!(classify("/etc", &ctx()).0, PathDanger::Catastrophic);
        assert_eq!(classify("/etc/hosts", &ctx()).0, PathDanger::Catastrophic,);
    }

    #[test]
    fn usr_local_is_not_catastrophic() {
        // `/usr` is exact-only: `/usr/local/bin` is a real install
        // target.
        assert_ne!(
            classify("/usr/local/bin/tool", &ctx()).0,
            PathDanger::Catastrophic,
        );
    }

    #[cfg(unix)]
    #[test]
    fn users_root_is_catastrophic_but_a_sibling_home_is_not() {
        // Deleting `/Users` removes every account's home; deleting
        // `/Users/other/project` is an ordinary path.
        assert_eq!(classify("/Users", &ctx()).0, PathDanger::Catastrophic);
        assert_eq!(classify("/home", &ctx()).0, PathDanger::Catastrophic);
        assert_ne!(
            classify("/Users/other/project/x.rs", &ctx()).0,
            PathDanger::Catastrophic,
        );
    }

    #[test]
    fn home_directory_itself_is_catastrophic() {
        assert_eq!(classify("/Users/dev", &ctx()).0, PathDanger::Catastrophic,);
    }

    #[test]
    fn working_dir_and_scratch_are_safe() {
        assert_eq!(classify("/work/proj/src/x.rs", &ctx()).0, PathDanger::Safe,);
        assert_eq!(classify("/tmp/build.log", &ctx()).0, PathDanger::Safe);
    }

    #[test]
    fn glob_over_ssh_is_catastrophic() {
        assert_eq!(
            classify("/Users/dev/.ssh/*", &ctx()).0,
            PathDanger::Catastrophic,
        );
    }

    #[cfg(unix)]
    #[test]
    fn device_node_is_catastrophic() {
        assert_eq!(classify("/dev/sda", &ctx()).0, PathDanger::Catastrophic,);
        assert_ne!(classify("/dev/null", &ctx()).0, PathDanger::Catastrophic);
    }

    #[cfg(unix)]
    #[test]
    fn dot_dot_out_of_a_protected_dir_is_caught() {
        // `~/.ssh/../../..` normalizes to `/Users`, not the store,
        // but the *operation* still targets the store's parent —
        // which is the home dir, caught by its own rule.
        assert_eq!(classify("~/.ssh/../..", &ctx()).0, PathDanger::Catastrophic,);
    }
}

#[cfg(test)]
mod paths_proptests {
    use super::*;
    use proptest::prelude::*;

    fn ctx() -> RiskContext {
        RiskContext {
            working_dir: PathBuf::from("/work/repo"),
            home_dir: PathBuf::from("/home/user"),
            scratch_dir: PathBuf::from("/tmp/scratch"),
        }
    }

    fn arb_base_components() -> impl Strategy<Value = Vec<String>> {
        prop::collection::vec(
            prop_oneof![
                Just("a".to_string()),
                Just("b".to_string()),
                Just("etc".to_string()),
                Just(".ssh".to_string()),
                Just(".git".to_string()),
                Just("user".to_string()),
            ],
            1..5,
        )
    }

    /// Insert a canceling fragment (`.` or `p/..`) before each base
    /// component. The two paths normalize to the same thing by
    /// construction — that is what the normalized-form property needs,
    /// and generating it directly avoids proptest rejecting ~99% of
    /// cases.
    fn same_normalized_pair() -> impl Strategy<Value = (String, String)> {
        (
            arb_base_components(),
            prop::collection::vec(
                prop_oneof![Just(".".to_string()), Just("p/..".to_string())],
                0..5,
            ),
        )
            .prop_map(|(base, noise)| {
                let a = base.join("/");
                let mut b_parts: Vec<String> = Vec::new();
                for (i, c) in base.iter().enumerate() {
                    if let Some(n) = noise.get(i) {
                        b_parts.push(n.clone());
                    }
                    b_parts.push(c.clone());
                }
                let b = b_parts.join("/");
                (a, b)
            })
    }

    proptest! {
        #[test]
        fn prop_normalize_has_no_dot_components(base in arb_base_components()) {
            let raw = base.join("/");
            let n = normalize(&raw);
            for c in n.components() {
                prop_assert!(
                    !matches!(c, Component::CurDir | Component::ParentDir),
                    "normalize({:?}) = {:?} retained {:?}",
                    raw, n, c,
                );
            }
        }

        #[test]
        fn prop_normalize_is_idempotent(base in arb_base_components()) {
            let raw = base.join("/");
            let once = normalize(&raw);
            let twice = normalize(&once.to_string_lossy());
            prop_assert_eq!(once, twice);
        }

        #[cfg(unix)]
        #[test]
        fn prop_normalize_absolute_stays_absolute(base in arb_base_components()) {
            let raw = format!("/{}", base.join("/"));
            let n = normalize(&raw);
            prop_assert!(n.is_absolute(), "normalize({:?}) = {:?} lost root", raw, n);
        }

        #[test]
        fn prop_classify_never_panics(raw in "\\PC{0,200}") {
            let _ = classify(&raw, &ctx());
        }

        #[test]
        fn prop_classify_is_deterministic(raw in "\\PC{0,200}") {
            let c = ctx();
            prop_assert_eq!(classify(&raw, &c), classify(&raw, &c));
        }

        /// The security invariant: the DANGER LEVEL is a pure function
        /// of the normalized path. `b` is built from `a` by inserting
        /// only canceling fragments, so the two normalized forms are
        /// equal by construction. The reason string embeds the raw
        /// input, so only the danger level is compared.
        #[test]
        fn prop_classify_danger_depends_only_on_normalized_form(
            (a, b) in same_normalized_pair(),
        ) {
            let c = ctx();
            prop_assert_eq!(normalize(&a), normalize(&b), "pair not equal after normalize: a={:?} b={:?}", a, b);
            prop_assert_eq!(
                classify(&a, &c).0,
                classify(&b, &c).0,
                "same normalized path, different verdict: a={:?} b={:?}",
                a, b,
            );
        }

        #[test]
        fn prop_ssh_is_never_safe(suffix in "[a-z]{0,8}") {
            let raw = format!("~/.ssh/{}", suffix);
            let (d, why) = classify(&raw, &ctx());
            prop_assert_ne!(d, PathDanger::Safe, "{:?} classified Safe ({})", raw, why);
        }

        #[test]
        fn prop_etc_with_traversal_is_never_safe(prefix in "(\\.\\.?/){0,4}") {
            let raw = format!("/tmp/{}etc/passwd", prefix);
            let (d, why) = classify(&raw, &ctx());
            prop_assert_ne!(d, PathDanger::Safe, "{:?} classified Safe ({})", raw, why);
        }
    }

    #[test]
    fn collapse_to_empty_becomes_root() {
        assert_eq!(normalize(""), PathBuf::from("/"));
        assert_eq!(normalize("a/.."), PathBuf::from("/"));
    }

    #[test]
    fn bare_parent_traversal_is_catastrophic() {
        let (d, _) = classify("..", &ctx());
        assert_eq!(d, PathDanger::Catastrophic);
    }

    #[cfg(unix)]
    #[test]
    fn traversal_into_etc_is_still_catastrophic() {
        let (d, _) = classify("/tmp/../etc/passwd", &ctx());
        assert_eq!(d, PathDanger::Catastrophic);
    }
}
