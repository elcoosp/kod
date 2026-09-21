//! Build a `ProviderRegistry` from the config's effective endpoints
//! (A4c, D1).
//!
//! This module is the composition root for LLM providers. Every
//! `set_provider` call site in the CLI and TUI migrates to calling
//! [`build_registry`] once at startup, then `set_registry`. The legacy
//! `set_provider` path stays for tests and for embedders that inject
//! their own provider directly.
//!
//! # Scope
//!
//! Two endpoint kinds are handled today:
//!
//! - `openai-compatible` — Ollama, LM Studio, vLLM, MLX, OpenAI, and
//!   every other server that speaks the `/chat/completions` protocol.
//!   Constructed via [`OpenAICompatProvider`].
//! - `anthropic` — returns an error today; the native provider lands
//!   in A5. The error message names the PR that will fix it, so a
//!   user running a v2 config before A5 sees an honest "not yet"
//!   instead of a silent fallback to OpenAI protocol that Anthropic
//!   would reject.
//!
//! # Default model selection
//!
//! The registry does not decide which endpoint serves the first
//! prompt — the caller does, via the returned `ModelRef`. The
//! convention is "the endpoint the config routes `Simple` tasks to",
//! falling back to the first endpoint in the effective list, which
//! for a v1 config is always the synthesised `"default"`.

use kod_config::{EndpointConfig, LlmConfig, ProviderKind};
use kod_error::{KodError, Result};
use kod_provider::{ModelRef, ProviderRegistry};
use kod_provider_openai::OpenAICompatProvider;
use std::sync::Arc;

/// Build a registry from a config's effective endpoints, plus the
/// `ModelRef` the caller should pass to `set_registry` as the initial
/// endpoint.
///
/// `model_override` replaces the model name on the returned default
/// `ModelRef` — the CLI uses this for `--model <name>` and the TUI
/// for the `/model` argument. Endpoints other than the default are
/// unaffected: the override applies to the initial route, not to
/// every endpoint in the registry.
pub fn build_registry(
    llm: &LlmConfig,
    model_override: Option<&str>,
) -> Result<(
    Arc<ProviderRegistry>,
    ModelRef,
    Option<kod_config::RoutingConfig>,
)> {
    // Endpoints come straight from the config; a v2 config always
    // carries them. An empty list is invalid (validate() reports it,
    // and Default synthesises one), so this is a defensive check.
    if llm.endpoints.is_empty() {
        return Err(KodError::Config(
            "llm.endpoints is empty; add at least one [[llm.endpoints]] block".to_string(),
        ));
    }
    let endpoints = llm.endpoints.clone();
    let mut registry = ProviderRegistry::new();

    for endpoint in &endpoints {
        let provider = build_provider(endpoint)?;
        // Capabilities are per-provider-kind plus the pricing the
        // user configured on the endpoint. The conservative matrix
        // covers the rest; a provider that reports its own matrix
        // (A5+) overrides here.
        let mut caps = kod_provider::ProviderCapabilities::conservative();
        if let Some(p) = &endpoint.pricing {
            caps.pricing = Some(kod_provider::ModelPricing::new(
                p.input_per_mtok_usd,
                p.output_per_mtok_usd,
            ));
        }
        registry.insert(
            endpoint.name.clone(),
            provider,
            caps,
            endpoint.model.clone(),
        );
    }

    // Default endpoint: the one routed for `Simple` tasks, or the
    // first in the effective list (which for a v1 synthesis is
    // always `"default"`).
    let default_name = llm.route_for_task("Simple");
    let default_endpoint = endpoints
        .iter()
        .find(|e| e.name == default_name)
        .or_else(|| endpoints.first())
        .ok_or_else(|| KodError::Config("llm config produced zero endpoints".to_string()))?;

    let model = model_override
        .unwrap_or(&default_endpoint.model)
        .to_string();
    let default_ref = ModelRef::new(default_endpoint.name.clone(), model);

    Ok((Arc::new(registry), default_ref, llm.routing.clone()))
}

/// Construct one provider from an endpoint config.
///
/// The API key is resolved from `api_key_env` first (an explicit
/// variable name in the config), then from `OPENAI_API_KEY` for the
/// OpenAI-compatible case, then a dummy value the local servers
/// accept. See `OpenAICompatProvider::with_api_key`.
fn build_provider(endpoint: &EndpointConfig) -> Result<Arc<dyn kod_provider::LlmProvider>> {
    match endpoint.provider {
        ProviderKind::OpenAICompatible => {
            let api_key = resolve_api_key(endpoint);
            let provider = OpenAICompatProvider::with_api_key_and_timeout(
                endpoint.base_url.clone(),
                endpoint.model.clone(),
                api_key,
                endpoint.timeout_secs,
            )?;
            Ok(Arc::new(provider))
        }
        ProviderKind::Anthropic => {
            let api_key = resolve_anthropic_api_key(endpoint)?;
            let provider = kod_provider_anthropic::AnthropicProvider::with_api_key(
                endpoint.base_url.clone(),
                endpoint.model.clone(),
                api_key,
            )?;
            Ok(Arc::new(provider))
        }
    }
}

/// Anthropic key resolution: `api_key_env` if set, then
/// `ANTHROPIC_API_KEY`. Unlike the OpenAI-compatible path, there is no
/// dummy fallback — Anthropic requires a real key, and a missing key
/// is a startup error with a fix-naming message.
fn resolve_anthropic_api_key(endpoint: &EndpointConfig) -> Result<String> {
    if let Some(var) = &endpoint.api_key_env
        && let Ok(value) = std::env::var(var)
        && !value.trim().is_empty()
    {
        return Ok(value);
    }
    if let Ok(value) = std::env::var("ANTHROPIC_API_KEY")
        && !value.trim().is_empty()
    {
        return Ok(value);
    }
    Err(KodError::Config(format!(
        "endpoint {:?}: no Anthropic API key. Set {} in the environment          (or set `api_key_env` on the endpoint to name a different          variable).",
        endpoint.name,
        endpoint
            .api_key_env
            .as_deref()
            .unwrap_or("ANTHROPIC_API_KEY"),
    )))
}

fn resolve_api_key(endpoint: &EndpointConfig) -> String {
    if let Some(var) = &endpoint.api_key_env
        && let Ok(value) = std::env::var(var)
        && !value.trim().is_empty()
    {
        return value;
    }
    if let Ok(value) = std::env::var("OPENAI_API_KEY")
        && !value.trim().is_empty()
    {
        return value;
    }
    // Same fallback the provider itself uses for local servers.
    "not-needed".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    /// Serializes the two tests that mutate `ANTHROPIC_API_KEY`. Rust
    /// runs test functions in parallel by default, so without this
    /// lock the "missing key" test can clear the var while the
    /// "present key" test is about to read it.
    fn anthropic_env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    /// A v1-shaped configuration, built in code. The v1 flat `[llm]`
    /// shape — `provider` / `model` / `base_url` at the top level of
    /// the section — cannot be deserialized into the v2 `LlmConfig`:
    /// those fields now live on `EndpointConfig`, and a v1 file needs
    /// `kod config migrate` before it loads. Building the struct
    /// programmatically exercises the same `build_registry` path with
    /// a deterministic single-endpoint config.
    fn v1_config() -> LlmConfig {
        let mut cfg = LlmConfig::default();
        {
            let ep = cfg.default_endpoint_mut();
            ep.provider = ProviderKind::OpenAICompatible;
            ep.model = "qwen2.5-coder:7b".to_string();
            ep.base_url = "http://localhost:11434/v1".to_string();
            ep.context_window = 8192;
            ep.max_tokens = Some(2048);
            ep.temperature = Some(0.2);
            ep.timeout_secs = 120;
        }
        cfg
    }

    #[test]
    fn v1_config_produces_one_registered_default_endpoint() {
        let cfg = v1_config();
        let (registry, default_ref, _routing) = build_registry(&cfg, None).unwrap();
        assert_eq!(registry.len(), 1);
        assert!(registry.has("default"));
        assert_eq!(default_ref.endpoint, "default");
        assert_eq!(default_ref.model, "qwen2.5-coder:7b");
    }

    #[test]
    fn model_override_applies_to_default_ref_only() {
        let cfg = v1_config();
        let (_registry, default_ref, _routing) = build_registry(&cfg, Some("llama3.1")).unwrap();
        assert_eq!(default_ref.model, "llama3.1");
    }

    #[test]
    fn anthropic_endpoint_requires_an_api_key() {
        let _guard = anthropic_env_lock();
        // Ensure no leaked env var from the environment turns this into
        // an accidental success.
        // SAFETY: serialized via anthropic_env_lock.
        unsafe { std::env::remove_var("ANTHROPIC_API_KEY") };

        let cfg: LlmConfig = toml::from_str(
            r#"
            provider = "OpenAICompatible"
            model = "unused"
            base_url = "http://localhost:11434/v1"
            context_window = 8192
            max_tokens = 2048
            temperature = 0.7
            timeout_secs = 300

            [[endpoints]]
            name = "cloud"
            provider = "anthropic"
            base_url = "https://api.anthropic.com"
            model = "claude-sonnet-4-5"
            context_window = 200000
            "#,
        )
        .unwrap();
        // Arc<ProviderRegistry> is not Debug (it contains
        // Arc<dyn LlmProvider>), so unwrap_err cannot be called.
        // Match instead.
        let err = match build_registry(&cfg, None) {
            Ok(_) => panic!("missing API key must error"),
            Err(e) => e,
        };
        let msg = err.to_string();
        assert!(msg.contains("Anthropic API key"), "got: {msg}");
        assert!(msg.contains("ANTHROPIC_API_KEY"), "got: {msg}");
    }

    #[test]
    fn anthropic_endpoint_builds_when_api_key_is_present() {
        let _guard = anthropic_env_lock();
        // SAFETY: serialized via anthropic_env_lock.
        unsafe { std::env::set_var("ANTHROPIC_API_KEY", "sk-ant-test-only") };

        let cfg: LlmConfig = toml::from_str(
            r#"
            provider = "OpenAICompatible"
            model = "unused"
            base_url = "http://localhost:11434/v1"
            context_window = 8192
            max_tokens = 2048
            temperature = 0.7
            timeout_secs = 300

            [[endpoints]]
            name = "cloud"
            provider = "anthropic"
            base_url = "https://api.anthropic.com"
            model = "claude-sonnet-4-5"
            context_window = 200000
            "#,
        )
        .unwrap();
        let (registry, default_ref, _routing) = build_registry(&cfg, None).unwrap();
        // The registry contains one endpoint named "cloud", and the
        // default ref follows the "Simple" route (absent → "default"
        // fallback is overridden by the first endpoint since there is
        // only one).
        assert!(registry.has("cloud"));
        assert_eq!(default_ref.endpoint, "cloud");
        assert_eq!(default_ref.model, "claude-sonnet-4-5");

        unsafe { std::env::remove_var("ANTHROPIC_API_KEY") };
    }

    #[test]
    fn routing_table_selects_the_default_endpoint() {
        let cfg: LlmConfig = toml::from_str(
            r#"
            provider = "OpenAICompatible"
            model = "x"
            base_url = "http://localhost:11434/v1"
            context_window = 8192
            max_tokens = 2048
            temperature = 0.7
            timeout_secs = 300

            [[endpoints]]
            name = "alpha"
            provider = "openai-compatible"
            base_url = "http://alpha.example/v1"
            model = "m-alpha"
            context_window = 8192

            [[endpoints]]
            name = "beta"
            provider = "openai-compatible"
            base_url = "http://beta.example/v1"
            model = "m-beta"
            context_window = 8192

            [routing.by_task]
            Simple = "beta"
            "#,
        )
        .unwrap();
        let (registry, default_ref, _routing) = build_registry(&cfg, None).unwrap();
        assert_eq!(registry.len(), 2);
        assert_eq!(default_ref.endpoint, "beta");
        assert_eq!(default_ref.model, "m-beta");
    }
}
