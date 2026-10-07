//! Preset × tool decision matrix (design §11.3 "matrice policy").
//!
//! `preset_decision` classifies a tool into `Allow` / `Ask` / `Deny`
//! under each preset. That classification is a security-relevant
//! invariant: a change that flipped `write_file` from `Ask` to `Allow`
//! under `Standard` would silently re-enable unsupervised writes with
//! every existing test still green. This file pins the full table.
//!
//! The table is deliberately explicit rather than derived: the point of
//! a matrix test is to be the reference the code must match, not to
//! reproduce the code's logic.

use kod_config::{Decision, KodConfig, PolicyEngine, Preset};
use std::collections::HashSet;

fn engine(preset: Preset) -> PolicyEngine {
    // Force the preset through the CLI-override layer — the same layer
    // a user reaches with `--preset`. Everything else is the built-in
    // default, so a regression in the default layer would also show up
    // in the `defaults` test below.
    PolicyEngine::load(&KodConfig::default(), None, Some(preset)).expect("policy load")
}

fn decide(e: &PolicyEngine, tool: &str) -> Decision {
    let empty: HashSet<kod_config::SessionDeny> = HashSet::new();
    e.decide(tool, &serde_json::json!({}), &std::env::temp_dir(), &empty)
        .outcome
}

/// The read-only set, spelled out. If a future tool joins the read-only
/// category, add it here on purpose — not by loosening the assertion.
const READ_ONLY_TOOLS: &[&str] = &[
    "read_file",
    "list_files",
    "grep",
    "search_files",
    "file_info",
    "git_status",
    "git_diff",
    "lsp_diagnostics",
    "lsp_definition",
    "lsp_references",
    "lsp_hover",
    "check",
];

/// The mutating set — every tool that changes the world (filesystem,
/// shell, network fetch, memory write) or that reaches outside the
/// project's read surface.
const MUTATING_TOOLS: &[&str] = &[
    "write_file",
    "patch_file",
    "execute_command",
    "web_fetch",
    "memory_save",
    "git_commit",
    "git_branch",
];

#[test]
fn read_only_preset_allows_reads_denies_writes() {
    let e = engine(Preset::ReadOnly);
    for t in READ_ONLY_TOOLS {
        assert_eq!(
            decide(&e, t),
            Decision::Allow,
            "ReadOnly must allow read-only tool {t}",
        );
    }
    for t in MUTATING_TOOLS {
        assert_eq!(
            decide(&e, t),
            Decision::Deny,
            "ReadOnly must deny mutating tool {t}",
        );
    }
}

#[test]
fn standard_preset_allows_reads_asks_for_writes() {
    let e = engine(Preset::Standard);
    for t in READ_ONLY_TOOLS {
        assert_eq!(
            decide(&e, t),
            Decision::Allow,
            "Standard must allow read-only tool {t}",
        );
    }
    for t in MUTATING_TOOLS {
        assert_eq!(
            decide(&e, t),
            Decision::Ask,
            "Standard must ask for mutating tool {t}",
        );
    }
}

#[test]
fn yolo_preset_allows_everything() {
    let e = engine(Preset::Yolo);
    for t in READ_ONLY_TOOLS.iter().chain(MUTATING_TOOLS.iter()) {
        assert_eq!(
            decide(&e, t),
            Decision::Allow,
            "Yolo must allow every tool, got {:?} for {t}",
            decide(&e, t),
        );
    }
}

#[test]
fn unknown_tool_falls_back_to_preset_default() {
    // An MCP tool, a plugin tool — anything the preset table does not
    // enumerate — is a mutating-class default: it might do anything,
    // so the conservative preset treats it the way it treats an
    // unknown write.
    let e = Preset::Standard;
    let std_engine = engine(e);
    assert_eq!(
        decide(&std_engine, "mcp:custom.do_something"),
        Decision::Ask,
        "an unknown tool must default to Ask under Standard",
    );
    let ro_engine = engine(Preset::ReadOnly);
    assert_eq!(
        decide(&ro_engine, "mcp:custom.do_something"),
        Decision::Deny,
        "an unknown tool must default to Deny under ReadOnly",
    );
}

/// The default-config layering: no `--preset`, no `[tools] preset`,
/// no project policy — the effective preset is `Standard`.
#[test]
fn default_layering_is_standard() {
    let e = PolicyEngine::load(&KodConfig::default(), None, None).expect("policy load");
    assert_eq!(e.effective().preset, Preset::Standard);
}

/// `[tools] preset = "read-only"` in the global config is honoured.
/// Regression guard for the earlier state where `PolicyEngine::load`
/// ignored the config entirely (`let _ = cfg;`).
#[test]
fn global_config_preset_is_honoured() {
    let mut cfg = KodConfig::default();
    cfg.tools.preset = Some("read-only".to_string());
    let e = PolicyEngine::load(&cfg, None, None).expect("policy load");
    assert_eq!(
        e.effective().preset,
        Preset::ReadOnly,
        "[tools] preset = \"read-only\" must select the ReadOnly preset",
    );
}

/// `[tools] preset = "yolo"` restores the pre-policy behaviour.
#[test]
fn global_config_yolo_is_honoured() {
    let mut cfg = KodConfig::default();
    cfg.tools.preset = Some("yolo".to_string());
    let e = PolicyEngine::load(&cfg, None, None).expect("policy load");
    assert_eq!(e.effective().preset, Preset::Yolo);
}

/// An unrecognized `[tools] preset` value is a warning, not a failure;
/// the loader falls back to `Standard`. A typo in a config field must
/// not make the session unusable.
#[test]
fn unknown_preset_string_falls_back_to_standard() {
    let mut cfg = KodConfig::default();
    cfg.tools.preset = Some("paranoid".to_string());
    let e = PolicyEngine::load(&cfg, None, None).expect("policy load");
    assert_eq!(e.effective().preset, Preset::Standard);
}

/// The CLI `--preset` value overrides the global config value.
#[test]
fn cli_preset_overrides_global_config() {
    let mut cfg = KodConfig::default();
    cfg.tools.preset = Some("yolo".to_string());
    let e = PolicyEngine::load(&cfg, None, Some(Preset::ReadOnly)).expect("policy load");
    assert_eq!(
        e.effective().preset,
        Preset::ReadOnly,
        "the CLI --preset must win over the config's [tools] preset",
    );
}
