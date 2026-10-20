//! Sensitivity classification for endpoint routing (P7).
//!
//! A turn that touches `.env` or a private key is a data-policy
//! decision, not just a cost decision. The classification maps a
//! set of touched paths to three levels, and the routing gate
//! filters the chain to endpoints whose configured trust tier is
//! high enough.
//!
//! The classifier reads the same ReadProtection globs the policy
//! engine uses for secret detection, so a path the engine would
//! redact also constrains the routing.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sensitivity {
    /// No touched path is protected.
    Public,
    /// A dotfile, dot-directory, or `..` traversal reference.
    ///
    /// `..` is included for defensive reasons: a path with a
    /// parent-directory component may resolve outside the repo root,
    /// so routing it through `standard`-trust endpoints rather than
    /// `untrusted` ones is the safer default. The check that makes
    /// this true is `starts_with('.') && len > 1` in `classify`;
    /// `..` (length 2) matches it, `.` (length 1) does not.
    Internal,
    /// A path the policy engine read-protects or denies.
    Sensitive,
}

impl Sensitivity {
    pub fn label(&self) -> &'static str {
        match self {
            Sensitivity::Public => "public",
            Sensitivity::Internal => "internal",
            Sensitivity::Sensitive => "sensitive",
        }
    }
}

/// Classify a turn by the paths it is about to touch.
pub fn classify(
    paths: &[PathBuf],
    read_protected: impl Fn(&Path) -> bool,
    denied: impl Fn(&Path) -> bool,
) -> Sensitivity {
    if paths.is_empty() {
        return Sensitivity::Public;
    }
    let mut has_internal = false;
    for p in paths {
        if read_protected(p) || denied(p) {
            return Sensitivity::Sensitive;
        }
        if p.components().any(|c| {
            let s = c.as_os_str().to_string_lossy();
            s.starts_with('.') && s.len() > 1
        }) {
            has_internal = true;
        }
    }
    if has_internal {
        Sensitivity::Internal
    } else {
        Sensitivity::Public
    }
}

/// The minimum trust tier an endpoint must declare.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrustRequirement(pub Option<&'static str>);

impl TrustRequirement {
    pub fn for_sensitivity(s: Sensitivity) -> Self {
        match s {
            Sensitivity::Public => TrustRequirement(None),
            Sensitivity::Internal => TrustRequirement(Some("standard")),
            Sensitivity::Sensitive => TrustRequirement(Some("trusted")),
        }
    }

    /// An endpoint that declared no tier is treated as standard.
    pub fn satisfied_by(&self, tier: Option<&str>) -> bool {
        let Some(req) = self.0 else {
            return true;
        };
        let declared = tier.unwrap_or("standard");
        match req {
            "standard" => matches!(declared, "standard" | "trusted"),
            "trusted" => declared == "trusted",
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no(_: &Path) -> bool {
        false
    }

    #[test]
    fn empty_is_public() {
        assert_eq!(classify(&[], no, no), Sensitivity::Public);
    }

    #[test]
    fn ordinary_source_is_public() {
        assert_eq!(
            classify(&[PathBuf::from("crates/core/src/lib.rs")], no, no),
            Sensitivity::Public
        );
    }

    #[test]
    fn read_protected_is_sensitive() {
        let p = |x: &Path| x.ends_with(".env");
        assert_eq!(
            classify(&[PathBuf::from("config/.env")], p, no),
            Sensitivity::Sensitive
        );
    }

    #[test]
    fn denied_is_sensitive() {
        let d = |x: &Path| x.to_string_lossy().contains("secrets");
        assert_eq!(
            classify(&[PathBuf::from("secrets/api.key")], no, d),
            Sensitivity::Sensitive
        );
    }

    #[test]
    fn dotfile_is_internal() {
        assert_eq!(
            classify(&[PathBuf::from(".gitignore")], no, no),
            Sensitivity::Internal
        );
    }

    #[test]
    fn dot_dir_is_internal() {
        assert_eq!(
            classify(&[PathBuf::from("project/.config/x.toml")], no, no),
            Sensitivity::Internal
        );
    }

    #[test]
    fn sensitive_dominates() {
        let p = |x: &Path| x.ends_with(".env");
        assert_eq!(
            classify(
                &[PathBuf::from("crates/core/lib.rs"), PathBuf::from(".env")],
                p,
                no
            ),
            Sensitivity::Sensitive
        );
    }

    #[test]
    fn public_requirement_accepts_all() {
        let r = TrustRequirement::for_sensitivity(Sensitivity::Public);
        assert!(r.satisfied_by(None));
        assert!(r.satisfied_by(Some("untrusted")));
    }

    #[test]
    fn internal_rejects_untrusted() {
        let r = TrustRequirement::for_sensitivity(Sensitivity::Internal);
        assert!(r.satisfied_by(None));
        assert!(r.satisfied_by(Some("standard")));
        assert!(r.satisfied_by(Some("trusted")));
        assert!(!r.satisfied_by(Some("untrusted")));
    }

    #[test]
    fn sensitive_only_trusted() {
        let r = TrustRequirement::for_sensitivity(Sensitivity::Sensitive);
        assert!(!r.satisfied_by(None));
        assert!(!r.satisfied_by(Some("standard")));
        assert!(r.satisfied_by(Some("trusted")));
    }
}

#[cfg(test)]
mod sensitivity_proptests {
    //! Properties for `classify`.
    //!
    //! The function is small but security-adjacent: its output routes
    //! a turn to a subset of the endpoint chain (`Public` -> all,
    //! `Internal` -> standard+trusted, `Sensitive` -> trusted only).
    //! The existing example tests pin specific inputs; these
    //! properties pin the general shape.
    //!
    //! One ambiguity surfaced during this round and is pinned as a
    //! **control, not a fixed bug**: `..` (ParentDir, length 2,
    //! starts with `.`) is counted as a dot-component by the
    //! `starts_with('.') && len > 1` check, so `foo/../bar` is
    //! classified `Internal`. The `Sensitivity::Internal` docstring
    //! said "A dotfile or dot-directory", and `..` is neither. The
    //! docstring has been updated to include the `..` traversal case
    //! rather than change the code — a path with `..` may resolve
    //! outside the repo root, and `Internal` (stricter than Public)
    //! is the safer default. The `control_parent_dir_is_internal`
    //! test pins the current behavior so a future "fix" is a visible
    //! decision.

    use super::*;
    use proptest::prelude::*;
    use std::path::{Path, PathBuf};

    fn always_false(_: &Path) -> bool {
        false
    }

    /// A path with no dot-component and no `..`. Always Public
    /// under `always_false` closures.
    fn plain_path() -> impl Strategy<Value = PathBuf> {
        "[a-zA-Z][a-zA-Z0-9_/]{0,20}".prop_map(PathBuf::from)
    }

    /// A path with at least one dotfile component:
    /// `pre/.post/file`. Classified at least Internal.
    fn dotfile_path() -> impl Strategy<Value = PathBuf> {
        ("[a-zA-Z][a-zA-Z0-9_]{0,10}", "[a-zA-Z][a-zA-Z0-9_]{0,10}")
            .prop_map(|(pre, post)| PathBuf::from(format!("{pre}/.{post}/file")))
    }

    /// Either a plain or a dotfile path, for the order-independence
    /// and monotonicity properties where both kinds must appear.
    fn mixed_path() -> impl Strategy<Value = PathBuf> {
        prop_oneof![plain_path(), dotfile_path()]
    }

    fn rank(s: &Sensitivity) -> u8 {
        match s {
            Sensitivity::Public => 0,
            Sensitivity::Internal => 1,
            Sensitivity::Sensitive => 2,
        }
    }

    proptest! {
        /// Determinism: same path list, same result. The function
        /// takes closures; a future version that recorded into a
        /// shared cell would fail here.
        #[test]
        fn prop_deterministic(paths in prop::collection::vec(mixed_path(), 0..=5)) {
            prop_assert_eq!(
                classify(&paths, always_false, always_false),
                classify(&paths, always_false, always_false),
            );
        }

        /// Empty list -> Public. The `Internal` docstring's
        /// implied contract for "no path is dotfile or protected".
        #[test]
        fn prop_empty_is_public(_dummy in 0u8..1) {
            prop_assert_eq!(
                classify(&[], always_false, always_false),
                Sensitivity::Public,
            );
        }

        /// Any dotfile component -> at least Internal. The core
        /// contract from the `Internal` docstring: a path that
        /// touches a dotfile/dot-directory is not Public.
        #[test]
        fn prop_dotfile_implies_at_least_internal(
            p in dotfile_path(),
            extra in prop::collection::vec(plain_path(), 0..=3),
        ) {
            let mut all = extra;
            all.push(p);
            let s = classify(&all, always_false, always_false);
            prop_assert!(
                rank(&s) >= rank(&Sensitivity::Internal),
                "dotfile in path list must be at least Internal, got {:?}", s,
            );
        }

        /// Order independence: permuting the path list does not
        /// change the result. The current loop is order-independent
        /// by construction (only state is a boolean OR + early
        /// return on Sensitive); pinning it catches a future
        /// "return Internal immediately on first dotfile" that
        /// would still be order-independent, or a "last path wins"
        /// that wouldn't.
        #[test]
        fn prop_order_independent(paths in prop::collection::vec(mixed_path(), 0..=5)) {
            let mut reversed = paths.clone();
            reversed.reverse();
            prop_assert_eq!(
                classify(&paths, always_false, always_false),
                classify(&reversed, always_false, always_false),
            );
        }

        /// Monotonicity under set growth: adding a path never lowers
        /// the classification. Adding a plain path to a Public set
        /// keeps it Public; adding a dotfile path raises it to
        /// Internal. Never the reverse.
        #[test]
        fn prop_adding_a_path_never_lowers(
            existing in prop::collection::vec(mixed_path(), 0..=3),
            extra in mixed_path(),
        ) {
            let before = classify(&existing, always_false, always_false);
            let mut after_paths = existing;
            after_paths.push(extra);
            let after = classify(&after_paths, always_false, always_false);
            prop_assert!(
                rank(&after) >= rank(&before),
                "adding a path lowered classification: {:?} -> {:?}",
                before, after,
            );
        }
    }

    // ---- controls ------------------------------------------------------

    /// `..` (ParentDir) starts with `.` and has length 2, so the
    /// `starts_with('.') && len > 1` check counts it as a
    /// dot-component. `foo/../bar` -> Internal. The `Internal`
    /// docstring has been updated to include this case explicitly;
    /// the test pins the behavior so a future change to exclude `..`
    /// is a visible decision.
    #[test]
    fn control_parent_dir_is_internal() {
        assert_eq!(
            classify(&[PathBuf::from("foo/../bar")], always_false, always_false),
            Sensitivity::Internal,
        );
    }

    /// `.` (CurDir) has length 1, so it does **not** count as a
    /// dot-component. `./foo` -> Public. This is the case the
    /// `len > 1` check exists for.
    #[test]
    fn control_curdir_is_not_dotfile() {
        assert_eq!(
            classify(&[PathBuf::from("./foo")], always_false, always_false),
            Sensitivity::Public,
        );
    }

    /// A dotfile component anywhere in the path counts, not just the
    /// first: `foo/.bar/baz` -> Internal.
    #[test]
    fn control_deep_dotfile_is_internal() {
        assert_eq!(
            classify(&[PathBuf::from("foo/.bar/baz")], always_false, always_false),
            Sensitivity::Internal,
        );
    }

    /// An empty `PathBuf` has no components, so it is Public.
    /// Edge case worth pinning because `paths.is_empty()` is false
    /// (one element) but `p.components()` yields nothing.
    #[test]
    fn control_empty_path_is_public() {
        assert_eq!(
            classify(&[PathBuf::new()], always_false, always_false),
            Sensitivity::Public,
        );
    }
}
