#![cfg(test)]
use super::*;
use kod_core_state::sensitivity::Sensitivity;
use kod_provider::{ModelInfo, ModelRef};
use std::collections::HashMap;

fn mk(endpoint: &str) -> ModelRef {
    ModelRef::new(endpoint, "m")
}

fn trust(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[test]
fn public_sensitivity_keeps_every_endpoint() {
    let chain = vec![mk("a"), mk("b")];
    let map = trust(&[("a", "untrusted"), ("b", "untrusted")]);
    let out = KodEngine::filter_chain_by_trust(&chain, &map, Sensitivity::Public);
    assert_eq!(out.len(), 2, "Public has no trust requirement");
}

#[test]
fn internal_keeps_standard_and_trusted_only() {
    let chain = vec![mk("t"), mk("s"), mk("u")];
    let map = trust(&[("t", "trusted"), ("s", "standard"), ("u", "untrusted")]);
    let out = KodEngine::filter_chain_by_trust(&chain, &map, Sensitivity::Internal);
    let names: Vec<&str> = out.iter().map(|m| m.endpoint.as_str()).collect();
    assert!(names.contains(&"t"));
    assert!(names.contains(&"s"));
    assert!(!names.contains(&"u"), "untrusted must not serve Internal");
}

#[test]
fn sensitive_keeps_trusted_only() {
    let chain = vec![mk("t"), mk("s"), mk("u")];
    let map = trust(&[("t", "trusted"), ("s", "standard"), ("u", "untrusted")]);
    let out = KodEngine::filter_chain_by_trust(&chain, &map, Sensitivity::Sensitive);
    let names: Vec<&str> = out.iter().map(|m| m.endpoint.as_str()).collect();
    assert_eq!(names, vec!["t"], "only trusted clears Sensitive");
}

#[test]
fn endpoint_with_no_declared_tier_is_treated_as_standard() {
    let chain = vec![mk("bare")];
    let map = trust(&[]);
    // Internal allows standard → bare survives.
    let out = KodEngine::filter_chain_by_trust(&chain, &map, Sensitivity::Internal);
    assert_eq!(out.len(), 1);
    // Sensitive requires trusted → bare is dropped.
    let out = KodEngine::filter_chain_by_trust(&chain, &map, Sensitivity::Sensitive);
    assert!(out.is_empty());
}

#[test]
fn every_endpoint_failing_yields_empty_for_the_caller_to_handle() {
    let chain = vec![mk("u1"), mk("u2")];
    let map = trust(&[("u1", "untrusted"), ("u2", "untrusted")]);
    let out = KodEngine::filter_chain_by_trust(&chain, &map, Sensitivity::Sensitive);
    assert!(out.is_empty(), "caller falls back to unfiltered on empty");
}

#[test]
fn order_is_preserved() {
    let chain = vec![mk("a"), mk("b"), mk("c")];
    let map = trust(&[("a", "trusted"), ("b", "trusted"), ("c", "trusted")]);
    let out = KodEngine::filter_chain_by_trust(&chain, &map, Sensitivity::Sensitive);
    let names: Vec<&str> = out.iter().map(|m| m.endpoint.as_str()).collect();
    assert_eq!(names, vec!["a", "b", "c"]);
}

#[test]
fn budget_hint_prefers_the_catalog_window_over_the_endpoint_config() {
    // The catalog path is pure: given a populated catalog, the
    // returned window is the model's, not the endpoint's. This
    // catches the class of bug where the catalog is populated but
    // never consulted.
    let temp = tempfile::TempDir::new().unwrap();
    std::fs::write(temp.path().join("main.rs"), "pub fn main() {}\n").unwrap();
    let db = temp.path().join("test.redb");
    let cfg = crate::router::RouterConfig {
        embedder: None,
        skill_threshold: 0.3,
        context_window: 8192,
        short_term_capacity: 100,
        working_dir: temp.path().to_path_buf(),
        enable_memory: false,
        max_skills_per_query: 3,
    };
    let engine = crate::engine::KodEngine::new(cfg, db).unwrap();

    engine.record_model_catalog(
        "ep",
        &[ModelInfo {
            id: "big".to_string(),
            context_window: Some(500_000),
            input_per_mtok_usd: None,
            output_per_mtok_usd: None,
            efforts: None,
            intelligence: None,
            tps: None,
            long_context: None,
        }],
    );

    let (window, _max_out) = engine.budget_hint_for(&ModelRef::new("ep", "big"));
    assert_eq!(window, 500_000, "catalog window must win");
}

#[test]
fn budget_hint_falls_back_when_the_model_is_not_in_the_catalog() {
    let temp = tempfile::TempDir::new().unwrap();
    std::fs::write(temp.path().join("main.rs"), "pub fn main() {}\n").unwrap();
    let db = temp.path().join("test.redb");
    let cfg = crate::router::RouterConfig {
        embedder: None,
        skill_threshold: 0.3,
        context_window: 8192,
        short_term_capacity: 100,
        working_dir: temp.path().to_path_buf(),
        enable_memory: false,
        max_skills_per_query: 3,
    };
    let engine = crate::engine::KodEngine::new(cfg, db).unwrap();

    engine.record_model_catalog(
        "ep",
        &[ModelInfo {
            id: "known".to_string(),
            context_window: Some(500_000),
            input_per_mtok_usd: None,
            output_per_mtok_usd: None,
            efforts: None,
            intelligence: None,
            tps: None,
            long_context: None,
        }],
    );

    // Ask about a different model — the catalog entry exists for
    // "known" but not for "unknown", so we should get the endpoint
    // config's window, not 500_000.
    let (window, _) = engine.budget_hint_for(&ModelRef::new("ep", "unknown"));
    assert_ne!(window, 500_000, "must not leak another model's window");
}
