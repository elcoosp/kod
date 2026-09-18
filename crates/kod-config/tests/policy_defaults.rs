//! The default preset and the CLI-preset-override contract.
//!
//! Regression guards for two linked changes (design §12 "Migration de
//! configuration" and AD-08):
//!
//! 1. The default policy preset is `Standard` — writes require
//!    approval — not the pre-policy `Yolo`. That is the design's
//!    headline behaviour change for D3 ("c'est le chantier contrôle").
//!    A regression that flipped it back would silently re-enable
//!    unsupervised writes, which is exactly the kind of change that
//!    must fail a test rather than ship.
//!
//! 2. When `--preset` is supplied, its **value** is honoured. The
//!    earlier `Some(_) => Standard, None => Yolo` was a bug: `--preset
//!    read-only` silently became `standard`, so a user asking for the
//!    safest preset got the middle one. This file pins both halves.
//!
//! The test does not need a project policy file; `PolicyEngine::load`
//! accepts `project_root: None`.

use kod_config::{Decision, KodConfig, PolicyEngine, Preset};
use std::collections::HashSet;

/// A working directory that exists. The engine resolves relative paths
/// against it; using `std::env::temp_dir()` means a path glob check
/// against `.env` etc. has a real anchor.
fn cwd() -> std::path::PathBuf {
    std::env::temp_dir()
}

fn decide(engine: &PolicyEngine, tool: &str) -> Decision {
    let empty: HashSet<kod_config::SessionDeny> = HashSet::new();
    engine
        .decide(tool, &serde_json::json!({}), &cwd(), &empty)
        .outcome
}

#[test]
fn default_preset_is_standard() {
    // No CLI override, no project policy file: the effective preset is
    // `Standard`, matching the design's §12 migration.
    let cfg = KodConfig::default();
    let engine = PolicyEngine::load(&cfg, None, None).expect("load");
    assert_eq!(
        engine.effective().preset,
        Preset::Standard,
        "default preset must be Standard (design §12) — a regression to \
         Yolo silently re-enables unsupervised writes",
    );
}

#[test]
fn cli_preset_read_only_is_honoured() {
    // Regression: `Some(_) => Standard` ignored the CLI's chosen value.
    let cfg = KodConfig::default();
    let engine =
        PolicyEngine::load(&cfg, None, Some(Preset::ReadOnly)).expect("load");
    assert_eq!(
        engine.effective().preset,
        Preset::ReadOnly,
        "--preset read-only must actually select ReadOnly, not silently \
         fall back to Standard",
    );
}

#[test]
fn cli_preset_yolo_is_honoured() {
    let cfg = KodConfig::default();
    let engine =
        PolicyEngine::load(&cfg, None, Some(Preset::Yolo)).expect("load");
    assert_eq!(engine.effective().preset, Preset::Yolo);
}

#[test]
fn standard_preset_asks_for_writes() {
    let cfg = KodConfig::default();
    let engine = PolicyEngine::load(&cfg, None, None).expect("load");
    assert_eq!(decide(&engine, "write_file"), Decision::Ask);
    assert_eq!(decide(&engine, "execute_command"), Decision::Ask);
}

#[test]
fn read_only_preset_denies_writes() {
    let cfg = KodConfig::default();
    let engine =
        PolicyEngine::load(&cfg, None, Some(Preset::ReadOnly)).expect("load");
    assert_eq!(decide(&engine, "write_file"), Decision::Deny);
    assert_eq!(decide(&engine, "execute_command"), Decision::Deny);
    // Reads stay allowed under every preset.
    assert_eq!(decide(&engine, "read_file"), Decision::Allow);
}

#[test]
fn yolo_preset_allows_everything() {
    let cfg = KodConfig::default();
    let engine =
        PolicyEngine::load(&cfg, None, Some(Preset::Yolo)).expect("load");
    assert_eq!(decide(&engine, "write_file"), Decision::Allow);
    assert_eq!(decide(&engine, "execute_command"), Decision::Allow);
}
