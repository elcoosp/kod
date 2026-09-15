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
    PRESETS.iter().map(|p| p.name).collect::<Vec<_>>().join(", ")
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
