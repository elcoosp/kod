//! Preset model profiles.
//!
//! "KOD works best with these four models. Here's the one-line install
//! for each." First-run friction is the single easiest adoption fix: a
//! user who has never run an agent should not have to know what a
//! `context_window` is to get one working.
//!
//! A profile is a small set of `LlmConfig` fields with a name and a
//! one-line description. `kod profile use <name>` writes the fields
//! into the user's `config.toml` under `[llm]`. The presets are
//! compiled in, so `kod profile list` works on a machine with no
//! config at all.

/// A named set of `[llm]` values.
#[derive(Debug, Clone, Copy)]
pub struct ModelProfile {
    pub name: &'static str,
    pub description: &'static str,
    pub model: &'static str,
    pub base_url: &'static str,
    pub context_window: usize,
    pub max_tokens: usize,
    pub install_command: Option<&'static str>,
}

/// Built-in profiles. Ordered by expected first-run usefulness: the
/// small local model first (fastest to pull), then larger locals, then
/// the cloud option last.
pub const PRESETS: &[ModelProfile] = &[
    ModelProfile {
        name: "local-fast",
        description: "Small local model (7B) for quick edits and questions",
        model: "qwen2.5-coder:7b",
        base_url: "http://localhost:11434/v1",
        context_window: 32768,
        max_tokens: 4096,
        install_command: Some("ollama pull qwen2.5-coder:7b"),
    },
    ModelProfile {
        name: "local-capable",
        description: "Larger local model (32B) for harder tasks — needs ~20 GB VRAM",
        model: "qwen2.5-coder:32b",
        base_url: "http://localhost:11434/v1",
        context_window: 32768,
        max_tokens: 4096,
        install_command: Some("ollama pull qwen2.5-coder:32b"),
    },
    ModelProfile {
        name: "local-reasoning",
        description: "Reasoning-focused local model (DeepSeek-R1 14B)",
        model: "deepseek-r1:14b",
        base_url: "http://localhost:11434/v1",
        context_window: 65536,
        max_tokens: 8192,
        install_command: Some("ollama pull deepseek-r1:14b"),
    },
    ModelProfile {
        name: "cloud-openai",
        description: "OpenAI cloud (requires OPENAI_API_KEY in the environment)",
        model: "gpt-4o-mini",
        base_url: "https://api.openai.com/v1",
        context_window: 128000,
        max_tokens: 4096,
        install_command: None,
    },
];

pub fn by_name(name: &str) -> Option<&'static ModelProfile> {
    PRESETS.iter().find(|p| p.name == name)
}

/// Comma-separated list of preset names, for an error message that
/// names the alternatives.
pub fn names_csv() -> String {
    PRESETS
        .iter()
        .map(|p| p.name)
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_have_unique_names() {
        let mut names: Vec<&str> = PRESETS.iter().map(|p| p.name).collect();
        names.sort();
        let mut deduped = names.clone();
        deduped.dedup();
        assert_eq!(names, deduped, "profile names must be unique");
    }

    #[test]
    fn by_name_finds_every_preset() {
        for p in PRESETS {
            assert_eq!(by_name(p.name).map(|x| x.name), Some(p.name));
        }
        assert!(by_name("no-such-profile").is_none());
    }

    #[test]
    fn every_local_preset_has_an_install_command() {
        for p in PRESETS {
            if p.base_url.contains("localhost") {
                assert!(
                    p.install_command.is_some(),
                    "local profile {:?} should name its install command",
                    p.name
                );
            }
        }
    }

    #[test]
    fn names_csv_lists_all() {
        let csv = names_csv();
        for p in PRESETS {
            assert!(csv.contains(p.name));
        }
    }
}

#[cfg(test)]
mod coverage_profile_fields {
    //! A profile is a small bundle of model settings `kod profile
    //! use` writes into the user's config. An empty or malformed
    //! field would produce a config the provider cannot reach —
    //! the failure shows up later, as a provider error, with no
    //! pointer back to the profile.
    use super::*;

    #[test]
    fn every_profile_has_non_empty_identity_fields() {
        for p in PRESETS {
            assert!(!p.name.is_empty(), "profile with empty name");
            assert!(
                !p.description.is_empty(),
                "profile {:?} with empty description",
                p.name,
            );
            assert!(
                !p.model.is_empty(),
                "profile {:?} with empty model",
                p.name,
            );
            assert!(
                !p.base_url.is_empty(),
                "profile {:?} with empty base_url",
                p.name,
            );
        }
    }

    #[test]
    fn every_profile_has_a_sane_context_window() {
        // A context window under 1000 tokens makes the session
        // unusable; over 10 million is a typo. Every shipped profile
        // must sit inside that range.
        for p in PRESETS {
            assert!(
                (1_000..=10_000_000).contains(&p.context_window),
                "profile {:?} context_window {} out of range",
                p.name,
                p.context_window,
            );
        }
    }

    #[test]
    fn every_profile_has_a_sane_max_tokens() {
        // `max_tokens` must be positive and no larger than the
        // context window — a max_tokens above the window is
        // nonsense the provider silently clamps.
        for p in PRESETS {
            assert!(
                p.max_tokens > 0,
                "profile {:?} has zero max_tokens",
                p.name,
            );
            assert!(
                p.max_tokens <= p.context_window,
                "profile {:?} max_tokens {} exceeds context_window {}",
                p.name,
                p.max_tokens,
                p.context_window,
            );
        }
    }

    #[test]
    fn base_urls_look_like_http_endpoints() {
        // Every shipped profile must have an http(s) base_url. A
        // profile with a bare hostname would fail at the first
        // provider call with an "invalid URL" error.
        for p in PRESETS {
            assert!(
                p.base_url.starts_with("http://") || p.base_url.starts_with("https://"),
                "profile {:?} base_url {:?} is not http(s)",
                p.name,
                p.base_url,
            );
        }
    }

    #[test]
    fn cloud_profile_has_no_install_command() {
        // The cloud preset is the only one that ships without an
        // install command: no local pull is required. A regression
        // that added a bogus install command would print a useless
        // hint to a user running `kod profile use cloud-openai`.
        let cloud = by_name("cloud-openai").expect("cloud profile shipped");
        assert!(cloud.install_command.is_none());
    }

    #[test]
    fn local_profiles_install_commands_are_ollama_pull() {
        // The install command is advisory, but the shipped locals
        // all pull from Ollama; a regression that dropped the
        // command would leave a user with no clear next step.
        for p in PRESETS {
            if p.base_url.contains("localhost") {
                let cmd = p.install_command.expect("local has install");
                assert!(
                    cmd.starts_with("ollama pull "),
                    "profile {:?} install {:?} is not an `ollama pull`",
                    p.name,
                    cmd,
                );
                // The pull target must match the model.
                assert!(
                    cmd.contains(p.model),
                    "profile {:?} install {:?} does not name the model {:?}",
                    p.name,
                    cmd,
                    p.model,
                );
            }
        }
    }

    #[test]
    fn names_csv_contains_every_name_separated_by_commas() {
        let csv = names_csv();
        for p in PRESETS {
            assert!(csv.contains(p.name), "{:?} missing from csv", p.name);
        }
        // Sanity: an N-profile library has N-1 separators.
        let commas = csv.matches(',').count();
        assert_eq!(commas, PRESETS.len() - 1);
    }
}
