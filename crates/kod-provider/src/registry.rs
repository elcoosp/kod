//! Named-endpoint registry (AD-06, D1).
//!
//! `ProviderRegistry` is the map the engine consults to turn a
//! `ModelRef { endpoint, model }` into an `Arc<dyn LlmProvider>`. It is
//! deliberately small: an immutable name -> (provider, capabilities)
//! table built once at startup from the config's endpoints list. All
//! the routing decisions (which task type goes to which endpoint,
//! fallback chains) live one layer up, in the `ConfigRouter` that A6
//! adds; the registry only answers "what provider speaks for this
//! endpoint name?".
//!
//! The registry does not know about `LlmConfig` or any other
//! kod-config type. Building it from `Vec<EndpointConfig>` is the
//! CLI/TUI's job (see A4c) — that is where both kod-config and the
//! provider crates are already in scope, so no crate gains a new
//! dependency for the sake of construction.

use crate::request::{ModelRef, ProviderCapabilities};
use crate::traits::LlmProvider;
use kod_error::{KodError, Result};
use std::collections::HashMap;
use std::sync::Arc;

/// One registered endpoint: the provider instance plus the capability
/// matrix the router/engine reads to adapt (streaming tool calls,
/// prompt-cache hints, pricing).
pub struct EndpointEntry {
    pub provider: Arc<dyn LlmProvider>,
    pub capabilities: ProviderCapabilities,
    /// The endpoint's model name (the `model` field of `EndpointConfig`).
    /// Stored here so `resolve_chain_for_task` can build `ModelRef`s
    /// without needing access to the config at runtime.
    pub default_model: String,
}

/// Endpoint-name -> (provider, capabilities).
pub struct ProviderRegistry {
    endpoints: HashMap<String, EndpointEntry>,
}

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self {
            endpoints: HashMap::new(),
        }
    }

    /// Register an endpoint. Idempotent: a second insert under the
    /// same name replaces the previous provider. This makes
    /// `set_registry` (engine) safe to call twice, and gives MCP
    /// hot-reload (D6.1) a natural shape if it ever needs to
    /// re-register.
    pub fn insert(
        &mut self,
        name: impl Into<String>,
        provider: Arc<dyn LlmProvider>,
        capabilities: ProviderCapabilities,
        default_model: impl Into<String>,
    ) {
        self.endpoints.insert(
            name.into(),
            EndpointEntry {
                provider,
                capabilities,
                default_model: default_model.into(),
            },
        );
    }

    /// The default model name configured for an endpoint.
    pub fn default_model(&self, endpoint: &str) -> Option<String> {
        self.endpoints
            .get(endpoint)
            .map(|e| e.default_model.clone())
    }

    /// The provider that speaks for `model_ref.endpoint`. The `model`
    /// field is validated for presence but not cross-checked against
    /// the provider — providers built from an `EndpointConfig` already
    /// carry their default model internally, and `GenerationOptions`
    /// does not (yet) let a caller override it per call.
    pub fn resolve(&self, model_ref: &ModelRef) -> Result<Arc<dyn LlmProvider>> {
        match self.endpoints.get(&model_ref.endpoint) {
            Some(entry) => Ok(Arc::clone(&entry.provider)),
            None => Err(KodError::InvalidState(format!(
                "unknown endpoint {:?}. Known: {:?}",
                model_ref.endpoint,
                self.names(),
            ))),
        }
    }

    /// Capabilities for an endpoint name, if registered.
    pub fn capabilities(&self, endpoint: &str) -> Option<ProviderCapabilities> {
        self.endpoints.get(endpoint).map(|e| e.capabilities)
    }

    /// Every registered endpoint name, sorted (stable order for
    /// `/model` completion and error messages).
    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.endpoints.keys().cloned().collect();
        names.sort();
        names
    }

    pub fn has(&self, endpoint: &str) -> bool {
        self.endpoints.contains_key(endpoint)
    }

    /// The first endpoint name in sorted order — a deterministic
    /// fallback for a caller that has no explicit routing decision
    /// (a v1 config synthesises a single "default" endpoint, so this
    /// returns it).
    pub fn default_endpoint(&self) -> Option<String> {
        self.names().into_iter().next()
    }

    pub fn is_empty(&self) -> bool {
        self.endpoints.is_empty()
    }

    pub fn len(&self) -> usize {
        self.endpoints.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::PromptCacheKind;
    use crate::traits::GenerationOptions;
    use crate::{GenerationResponse, StreamChunk};
    use async_trait::async_trait;
    use futures::Stream;
    use kod_types::ToolDefinition;
    use std::pin::Pin;

    struct Stub(&'static str);

    #[async_trait]
    impl LlmProvider for Stub {
        fn name(&self) -> &str {
            self.0
        }
        async fn list_models(&self) -> Result<Vec<crate::traits::ModelInfo>> {
            Ok(vec![])
        }
        async fn generate(&self, _p: &str, _o: &GenerationOptions) -> Result<String> {
            Ok(String::new())
        }
        async fn generate_with_tools(
            &self,
            _p: &str,
            _t: &[ToolDefinition],
            _o: &GenerationOptions,
        ) -> Result<GenerationResponse> {
            Ok(GenerationResponse::Text {
                content: String::new(),
                usage: None,
            })
        }
        fn stream(
            &self,
            _p: &str,
            _o: &GenerationOptions,
        ) -> Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send + '_>> {
            Box::pin(futures::stream::empty())
        }
    }

    fn caps() -> ProviderCapabilities {
        ProviderCapabilities {
            prompt_cache: PromptCacheKind::None,
            ..ProviderCapabilities::conservative()
        }
    }

    #[test]
    fn resolve_returns_the_registered_provider() {
        let mut r = ProviderRegistry::new();
        r.insert("local", Arc::new(Stub("local")), caps(), "test-model");
        r.insert("cloud", Arc::new(Stub("cloud")), caps(), "test-model");

        let p = r.resolve(&ModelRef::new("local", "any-model")).unwrap();
        assert_eq!(p.name(), "local");
        let p = r.resolve(&ModelRef::new("cloud", "any-model")).unwrap();
        assert_eq!(p.name(), "cloud");
    }

    #[test]
    fn resolve_unknown_endpoint_is_an_error_naming_the_known_set() {
        let mut r = ProviderRegistry::new();
        r.insert("local", Arc::new(Stub("local")), caps(), "test-model");
        // `Arc<dyn LlmProvider>` is not Debug, so `expect_err` (which
        // requires `T: Debug`) cannot be used. Match instead.
        let err = match r.resolve(&ModelRef::new("nope", "m")) {
            Ok(_) => panic!("unknown endpoint must error"),
            Err(e) => e,
        };
        let msg = err.to_string();
        assert!(msg.contains("nope"), "got: {msg}");
        assert!(msg.contains("local"), "got: {msg}");
    }

    #[test]
    fn insert_replaces_by_name() {
        let mut r = ProviderRegistry::new();
        r.insert("x", Arc::new(Stub("first")), caps(), "test-model");
        r.insert("x", Arc::new(Stub("second")), caps(), "test-model");
        assert_eq!(r.len(), 1);
        let p = r.resolve(&ModelRef::new("x", "m")).unwrap();
        assert_eq!(p.name(), "second");
    }

    #[test]
    fn names_are_sorted() {
        let mut r = ProviderRegistry::new();
        r.insert("z", Arc::new(Stub("z")), caps(), "test-model");
        r.insert("a", Arc::new(Stub("a")), caps(), "test-model");
        r.insert("m", Arc::new(Stub("m")), caps(), "test-model");
        assert_eq!(r.names(), vec!["a", "m", "z"]);
    }

    #[test]
    fn default_endpoint_is_first_in_sorted_order() {
        let mut r = ProviderRegistry::new();
        r.insert("zeta", Arc::new(Stub("z")), caps(), "test-model");
        r.insert("alpha", Arc::new(Stub("a")), caps(), "test-model");
        assert_eq!(r.default_endpoint().as_deref(), Some("alpha"));
    }
}
