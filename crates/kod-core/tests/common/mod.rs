//! Shared helpers for kod-core integration tests.
//!
//! The engine's production API installs providers through a
//! `ProviderRegistry` (see `KodEngine::set_registry`). Tests that only
//! want "this one provider serves every prompt" go through
//! [`install_test_provider`] rather than rebuilding a one-endpoint
//! registry inline at every call site.

#![allow(dead_code)]

use kod_core::KodEngine;
use kod_provider::{LlmProvider, ModelRef, ProviderCapabilities, ProviderRegistry};
use std::sync::Arc;

/// Install `provider` as the endpoint named `"default"`. Equivalent to
/// the pre-registry `engine.set_provider(provider)` shape: every
/// model reference resolves to this provider.
pub async fn install_test_provider(
    engine: &KodEngine,
    provider: Arc<dyn LlmProvider>,
) {
    let mut reg = ProviderRegistry::new();
    reg.insert(
        "default",
        provider,
        ProviderCapabilities::conservative(),
        "",
    );
    engine
        .set_registry(Arc::new(reg), ModelRef::new("default", ""), None)
        .await;
}

/// Install `provider` as an endpoint named `name`, with `model` as
/// its default model. For tests that need the two-endpoint setup
/// (registry + fallback chain) rather than the single-provider
/// shortcut.
pub async fn install_named_provider(
    engine: &KodEngine,
    name: &str,
    model: &str,
    provider: Arc<dyn LlmProvider>,
) {
    let mut reg = ProviderRegistry::new();
    reg.insert(
        name,
        provider,
        ProviderCapabilities::conservative(),
        model,
    );
    engine
        .set_registry(
            Arc::new(reg),
            ModelRef::new(name, model),
            None,
        )
        .await;
}
