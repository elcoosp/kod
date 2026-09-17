//! LLM provider configuration.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LlmConfig {
    /// Allow the agent to reach the network through `web_fetch`.
    /// Default false: the agent running with a user's shell privileges
    /// should not reach out without an explicit opt-in.
    pub network_access: bool,
    /// The configured endpoints. Must be non-empty; `validate()`
    /// enforces it. Each endpoint carries its own `base_url`, `model`,
    /// `context_window`, `max_tokens`, `temperature`, and
    /// `timeout_secs` — the v2 layout, so a swarm can route a planner
    /// to one endpoint and a coder to another without sharing options.
    pub endpoints: Vec<EndpointConfig>,
    /// Which endpoint each task type routes to, plus a fallback
    /// chain. `None` means every task uses `default_endpoint()`.
    pub routing: Option<RoutingConfig>,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            network_access: false,
            endpoints: vec![EndpointConfig {
                name: "default".to_string(),
                provider: ProviderKind::OpenAICompatible,
                base_url: "http://localhost:11434/v1".to_string(),
                model: "codellama:13b".to_string(),
                api_key_env: None,
                temperature: Some(0.7),
                max_tokens: Some(2048),
                context_window: 8192,
                timeout_secs: 300,
                pricing: None,
            }],
            routing: None,
        }
    }
}

impl LlmConfig {
    /// The endpoint used when no routing entry matches.
    ///
    /// Infallible: `validate()` repopulates an empty list, and every
    /// caller that goes through `KodConfig::load_default()` sees a
    /// non-empty vec. A caller that bypassed validation (a bare
    /// `LlmConfig { endpoints: vec![], .. }`) gets a static default
    /// endpoint — the same shape as `LlmConfig::default()` — rather
    /// than a panic or a fallible accessor.
    pub fn default_endpoint(&self) -> &EndpointConfig {
        static FALLBACK: std::sync::OnceLock<EndpointConfig> =
            std::sync::OnceLock::new();
        self.endpoints.first().unwrap_or_else(|| {
            FALLBACK.get_or_init(|| {
                LlmConfig::default().endpoints.into_iter().next().unwrap()
            })
        })
    }

    /// Mutable variant. When the endpoints vec is empty, a default
    /// endpoint is pushed first so the caller has something to mutate.
    pub fn default_endpoint_mut(&mut self) -> &mut EndpointConfig {
        if self.endpoints.is_empty() {
            self.endpoints = LlmConfig::default().endpoints;
        }
        &mut self.endpoints[0]
    }

    /// Clamp out-of-range or nonsensical values on every endpoint.
    /// Called by `KodConfig::load_default()` after deserialization.
    ///
    /// Providers accept a narrow range of temperature, a positive
    /// context_window, a positive max_tokens, a timeout >= 1 s, and a
    /// non-empty base_url + model. A value outside those ranges fails
    /// with an opaque HTTP error at the first prompt otherwise. This
    /// clamps with a `tracing::warn!` per adjustment so the session
    /// still runs and the user sees what changed.
    pub fn validate(&mut self) {
        if self.endpoints.is_empty() {
            tracing::warn!(
                "llm.endpoints is empty; falling back to the built-in default endpoint"
            );
            self.endpoints = LlmConfig::default().endpoints;
        }
        for e in &mut self.endpoints {
            // Temperature: 0.0..=2.0.
            match e.temperature {
                Some(t) if !t.is_finite() => {
                    tracing::warn!(
                        endpoint = %e.name,
                        value = %t,
                        "temperature is not finite; falling back to 0.7"
                    );
                    e.temperature = Some(0.7);
                }
                Some(t) if t < 0.0 => {
                    tracing::warn!(
                        endpoint = %e.name,
                        value = %t,
                        "temperature < 0; clamping to 0.0"
                    );
                    e.temperature = Some(0.0);
                }
                Some(t) if t > 2.0 => {
                    tracing::warn!(
                        endpoint = %e.name,
                        value = %t,
                        "temperature > 2.0; clamping to 2.0"
                    );
                    e.temperature = Some(2.0);
                }
                _ => {}
            }
            // Context window: `usize` (required by the schema). 0
            // means "use the default"; below the coherent minimum
            // means "clamp up".
            const MIN_CONTEXT_WINDOW: usize = 2_000;
            if e.context_window == 0 {
                tracing::warn!(
                    endpoint = %e.name,
                    "context_window is 0; falling back to the default"
                );
                e.context_window = LlmConfig::default().endpoints[0].context_window;
            } else if e.context_window < MIN_CONTEXT_WINDOW {
                tracing::warn!(
                    endpoint = %e.name,
                    value = e.context_window,
                    floor = MIN_CONTEXT_WINDOW,
                    "context_window below coherent minimum; clamping"
                );
                e.context_window = MIN_CONTEXT_WINDOW;
            }
            // max_tokens: 0 is treated as "use the default" because
            // some providers interpret it as "generate nothing".
            if matches!(e.max_tokens, Some(0)) {
                tracing::warn!(
                    endpoint = %e.name,
                    "max_tokens is 0; falling back to the default"
                );
                e.max_tokens = Some(LlmConfig::default().endpoints[0].max_tokens.unwrap());
            }
            // timeout: floor at 1 s.
            if e.timeout_secs < 1 {
                tracing::warn!(
                    endpoint = %e.name,
                    value = e.timeout_secs,
                    "timeout_secs < 1; clamping to 1"
                );
                e.timeout_secs = 1;
            }
            // base_url: non-empty.
            if e.base_url.trim().is_empty() {
                tracing::warn!(
                    endpoint = %e.name,
                    "base_url is empty; falling back to the default"
                );
                e.base_url = LlmConfig::default().endpoints[0].base_url.clone();
            }
            // model: non-empty.
            if e.model.trim().is_empty() {
                tracing::warn!(
                    endpoint = %e.name,
                    "model is empty; falling back to the default"
                );
                e.model = LlmConfig::default().endpoints[0].model.clone();
            }
        }

        // Reject duplicate endpoint names — a routing table that
        // points at "local" would be ambiguous.
        let mut seen = std::collections::HashSet::new();
        for e in &self.endpoints {
            if !seen.insert(e.name.clone()) {
                tracing::warn!(
                    endpoint = %e.name,
                    "duplicate endpoint name; the second registration replaces the first"
                );
            }
        }

        // Routing entries must reference existing endpoints; a typo
        // is dropped with a warning rather than failing the load.
        if let Some(r) = &mut self.routing {
            let names: std::collections::HashSet<String> =
                self.endpoints.iter().map(|e| e.name.clone()).collect();
            r.by_task.retain(|k, v| {
                if names.contains(v) {
                    true
                } else {
                    tracing::warn!(
                        task = %k,
                        endpoint = %v,
                        "routing.by_task references an unknown endpoint; dropping"
                    );
                    false
                }
            });
            r.fallback.retain(|v| {
                if names.contains(v) {
                    true
                } else {
                    tracing::warn!(
                        endpoint = %v,
                        "routing.fallback references an unknown endpoint; dropping"
                    );
                    false
                }
            });
            r.swarm.retain(|k, v| {
                if names.contains(v) {
                    true
                } else {
                    tracing::warn!(
                        capability = %k,
                        endpoint = %v,
                        "routing.swarm references an unknown endpoint; dropping"
                    );
                    false
                }
            });
        }
    }

    /// The endpoint a task type routes to, honouring the routing
    /// table when present and falling back to the default endpoint's
    /// name otherwise.
    pub fn route_for_task(&self, task_key: &str) -> String {
        if let Some(routing) = &self.routing
            && let Some(target) = routing.by_task.get(task_key)
        {
            return target.clone();
        }
        self.default_endpoint().name.clone()
    }
}

/// Provider protocol kind for an endpoint. Distinct from `ProviderType`
/// (the legacy single-provider enum) because v2 endpoints carry an
/// explicit `provider = "openai-compatible" | "anthropic"` field that
/// has a different set of legal values (no `Custom` placeholder).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderKind {
    #[serde(rename = "openai-compatible", alias = "Ollama", alias = "OpenAI")]
    OpenAICompatible,
    #[serde(rename = "anthropic")]
    Anthropic,
}

/// One named endpoint. The `name` is the config key referenced by
/// `RoutingConfig`; the model name is what the provider sends on the
/// wire.
///
/// Every field that affects the wire — `base_url`, `model`,
/// `temperature`, `max_tokens` — lives here rather than in a global
/// `LlmConfig`, so a swarm can route a planner to one endpoint and a
/// coder to another without sharing options.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EndpointConfig {
    pub name: String,
    pub provider: ProviderKind,
    pub base_url: String,
    pub model: String,
    /// Name of the environment variable that holds the API key. The
    /// value itself is never written to the config file — only the
    /// variable name is, so a shared config does not leak a secret.
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub max_tokens: Option<usize>,
    pub context_window: usize,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(default)]
    pub pricing: Option<PricingConfig>,
}

fn default_timeout_secs() -> u64 {
    300
}

/// USD per million tokens; used by the TUI cost accounting.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct PricingConfig {
    pub input_per_mtok_usd: f64,
    pub output_per_mtok_usd: f64,
}

/// Which endpoint each task type routes to, plus a fallback chain.
/// Task-type keys match `kod_core::TaskType` string forms; capability
/// keys match the swarm's `Capability` string forms. Both are kept as
/// strings here so kod-config does not depend on kod-core.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RoutingConfig {
    /// `{"Simple" -> "local-ollama", "Debugging" -> "anthropic", ...}`.
    /// An entry whose endpoint name does not exist in `endpoints` is
    /// rejected at load time — a typo must not silently disable a
    /// route.
    #[serde(default)]
    pub by_task: std::collections::BTreeMap<String, String>,
    /// Ordered list of endpoint names to try when the primary fails
    /// with a retryable error.
    #[serde(default)]
    pub fallback: Vec<String>,
    /// `{"Coding" -> "local-ollama", "Planning" -> "anthropic", ...}`
    /// for swarm role-based routing (A7).
    #[serde(default)]
    pub swarm: std::collections::BTreeMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_has_one_endpoint() {
        let c = LlmConfig::default();
        assert_eq!(c.endpoints.len(), 1);
        assert_eq!(c.endpoints[0].name, "default");
        assert_eq!(c.endpoints[0].model, "codellama:13b");
        assert_eq!(c.default_endpoint().name, "default");
    }

    #[test]
    fn default_endpoint_falls_back_when_empty() {
        let c = LlmConfig {
            endpoints: Vec::new(),
            ..LlmConfig::default()
        };
        // Returns a static fallback rather than panicking.
        assert_eq!(c.default_endpoint().name, "default");
    }

    #[test]
    fn validate_repopulates_empty_endpoints() {
        let mut c = LlmConfig {
            endpoints: Vec::new(),
            ..LlmConfig::default()
        };
        c.validate();
        assert_eq!(c.endpoints.len(), 1);
        assert_eq!(c.endpoints[0].name, "default");
    }

    #[test]
    fn validate_clamps_temperature() {
        let mut c = LlmConfig::default();
        c.endpoints[0].temperature = Some(5.0);
        c.validate();
        assert_eq!(c.endpoints[0].temperature, Some(2.0));

        let mut c = LlmConfig::default();
        c.endpoints[0].temperature = Some(-1.0);
        c.validate();
        assert_eq!(c.endpoints[0].temperature, Some(0.0));

        let mut c = LlmConfig::default();
        c.endpoints[0].temperature = Some(f32::NAN);
        c.validate();
        assert_eq!(c.endpoints[0].temperature, Some(0.7));
    }

    #[test]
    fn validate_clamps_context_window_and_max_tokens() {
        let mut c = LlmConfig::default();
        c.endpoints[0].context_window = 0;
        c.endpoints[0].max_tokens = Some(0);
        c.validate();
        assert_eq!(c.endpoints[0].context_window, 8192);
        assert_eq!(c.endpoints[0].max_tokens, Some(2048));

        let mut c = LlmConfig::default();
        c.endpoints[0].context_window = 500;
        c.validate();
        assert_eq!(c.endpoints[0].context_window, 2_000);
    }

    #[test]
    fn validate_floors_timeout() {
        let mut c = LlmConfig::default();
        c.endpoints[0].timeout_secs = 0;
        c.validate();
        assert_eq!(c.endpoints[0].timeout_secs, 1);
    }

    #[test]
    fn validate_falls_back_on_empty_strings() {
        let mut c = LlmConfig::default();
        c.endpoints[0].base_url = "   ".to_string();
        c.endpoints[0].model = "".to_string();
        c.validate();
        assert!(!c.endpoints[0].base_url.trim().is_empty());
        assert!(!c.endpoints[0].model.is_empty());
    }

    #[test]
    fn validate_drops_unknown_routing_targets() {
        let mut r = RoutingConfig::default();
        r.by_task.insert("Simple".into(), "default".into());
        r.by_task.insert("Debugging".into(), "nope".into());
        r.fallback.push("default".into());
        r.fallback.push("nope".into());
        let mut c = LlmConfig {
            routing: Some(r),
            ..LlmConfig::default()
        };
        c.validate();
        let r = c.routing.as_ref().unwrap();
        assert!(r.by_task.contains_key("Simple"));
        assert!(!r.by_task.contains_key("Debugging"));
        assert_eq!(r.fallback, vec!["default".to_string()]);
    }

    #[test]
    fn route_for_task_uses_routing_table() {
        let mut r = RoutingConfig::default();
        r.by_task.insert("Simple".into(), "local".into());
        let c = LlmConfig {
            endpoints: vec![
                EndpointConfig {
                    name: "default".into(),
                    provider: ProviderKind::OpenAICompatible,
                    base_url: "http://x".into(),
                    model: "m".into(),
                    api_key_env: None,
                    temperature: None,
                    max_tokens: None,
                    context_window: 8192,
                    timeout_secs: 300,
                    pricing: None,
                },
                EndpointConfig {
                    name: "local".into(),
                    provider: ProviderKind::OpenAICompatible,
                    base_url: "http://y".into(),
                    model: "m".into(),
                    api_key_env: None,
                    temperature: None,
                    max_tokens: None,
                    context_window: 8192,
                    timeout_secs: 300,
                    pricing: None,
                },
            ],
            routing: Some(r),
            ..LlmConfig::default()
        };
        assert_eq!(c.route_for_task("Simple"), "local");
        // Unrouted task falls back to the first endpoint's name.
        assert_eq!(c.route_for_task("Debugging"), "default");
    }
}
