// `install_test_provider` — shared test helper.
//
// Included by the test targets that use it via
// `#[path = "common/install_test_provider.rs"] mod install_test_provider_mod;`.
// Keeping each helper in its own file is what avoids the per-target
// dead-code warning: a test binary sees only the helpers it actually
// imports, so a helper that only one target uses is dead to no one.
//
// A `mod` file does not inherit the parent crate's `use` items, so
// each helper carries its own imports.

use kod_core::KodEngine;
use kod_provider::{LlmProvider, ModelRef, ProviderCapabilities, ProviderRegistry};
use std::sync::Arc;

/// Install a single-endpoint registry under the name `default` on
/// `engine`. Convenience for tests that need to inject a scripted
/// provider without building a registry by hand.
pub async fn install_test_provider(engine: &KodEngine, provider: Arc<dyn LlmProvider>) {
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
