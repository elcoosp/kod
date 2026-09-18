// `install_named_provider` — shared test helper.
//
// Same rationale as the sibling `install_test_provider.rs`: a per-file
// include keeps each helper scoped to the test binary that uses it.
//
// A `mod` file does not inherit the parent crate's `use` items, so
// this file carries its own imports.

use kod_core::KodEngine;
use kod_provider::{LlmProvider, ModelRef, ProviderCapabilities, ProviderRegistry};
use std::sync::Arc;

/// Install a single-endpoint registry under a caller-chosen endpoint
/// name. The `model_ref` names the `ModelRef` the engine's
/// `set_registry` should select as the initial model, so a test that
/// wants a non-default endpoint name does not have to fight the
/// default.
pub async fn install_named_provider(
    engine: &KodEngine,
    endpoint: &str,
    model_ref: ModelRef,
    provider: Arc<dyn LlmProvider>,
) {
    let mut reg = ProviderRegistry::new();
    reg.insert(
        endpoint,
        provider,
        ProviderCapabilities::conservative(),
        &model_ref.model,
    );
    engine.set_registry(Arc::new(reg), model_ref, None).await;
}
