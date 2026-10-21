//! Renderers in `observability.rs`, asserted as data.
//!
//! `run_limits_show` and `run_jev_tune_show` each called
//! `KodConfig::load_default()` and then printed a fixed-shape block.
//! Both are now a two-line wrapper over a `_lines(&cfg) -> Vec<String>`
//! helper, so an integration test can build a config, call the
//! helper, and assert on the content — no stdout capture, no
//! home-directory read.
use kod_cli::commands::observability::{jev_tune_lines, limits_lines};
use kod_config::KodConfig;

/// Every documented label appears in the limits block. The labels are
/// literal in the source, so this fails if a field is dropped from
/// the render (a real regression: a limit the user cannot see is a
/// limit they cannot reason about).
#[test]
fn limits_lines_contain_every_documented_label() {
    let cfg = KodConfig::default();
    let lines = limits_lines(&cfg);
    let joined = lines.join("\n");
    for label in [
        "max_cost_usd_per_session",
        "max_cost_usd_per_turn",
        "max_input_tokens_per_turn",
        "max_output_tokens_per_turn",
        "on_exhausted",
        "soft_warn_at",
    ] {
        assert!(
            joined.contains(label),
            "limits render is missing the {label:?} label:\n{joined}",
        );
    }
    assert_eq!(lines[0], "Limits", "first line must be the header");
}

/// Per-tool quotas appear only when tools are configured. A default
/// config has none, so the block is absent.
#[test]
fn limits_lines_omit_tool_block_when_no_tools() {
    let cfg = KodConfig::default();
    let lines = limits_lines(&cfg);
    let joined = lines.join("\n");
    assert!(
        !joined.contains("Per-tool quotas"),
        "default config should not print the tool-quota block:\n{joined}",
    );
}

/// `jev_tune_lines` always starts with the header and never panics on
/// a default config. Threshold entries are conditional on the
/// threshold having a value; the header is unconditional.
#[test]
fn jev_tune_lines_have_header() {
    let cfg = KodConfig::default();
    let lines = jev_tune_lines(&cfg);
    assert_eq!(lines[0], "Jev thresholds");
    // Every non-header line begins with two spaces and contains the
    // threshold name. Assert the shape, not a specific threshold.
    for line in &lines[1..] {
        assert!(
            line.starts_with("  "),
            "threshold line not indented: {line:?}",
        );
    }
}
