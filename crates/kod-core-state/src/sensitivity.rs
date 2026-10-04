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
    /// A dotfile or dot-directory.
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
