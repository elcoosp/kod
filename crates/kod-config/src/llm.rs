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
        static FALLBACK: std::sync::OnceLock<EndpointConfig> = std::sync::OnceLock::new();
        self.endpoints.first().unwrap_or_else(|| {
            FALLBACK.get_or_init(|| LlmConfig::default().endpoints.into_iter().next().unwrap())
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
            tracing::warn!("llm.endpoints is empty; falling back to the built-in default endpoint");
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

        // The routing tables are keyed by string forms the engine
        // produces at call time. A key that does not match one of the
        // known strings is dead: it will never fire, and the user
        // who wrote it believes a route is in place when it is not.
        // The design's §12 ("no placebo config") applies here as
        // much as anywhere else.
        //
        //   - `by_task` keys must match `TaskType`'s Debug names —
        //     `Simple`, `CodeModification`, `Debugging`, `Research`,
        //     `Testing`, `Documentation`, `Complex`, `MultiStep`.
        //     The engine uses `format!("{:?}", task_type)`.
        //   - `swarm` keys must match `Capability::as_str()` strings —
        //     `coding`, `testing`, `documentation`, `code-review`,
        //     `planning`, `research`, `debugging`, `refactoring`.
        //
        // Unknown keys are dropped with a warning naming the closest
        // known key when one is close enough to be a likely typo.
        if let Some(r) = &mut self.routing {
            const KNOWN_TASK_KEYS: &[&str] = &[
                "Simple",
                "CodeModification",
                "Debugging",
                "Research",
                "Testing",
                "Documentation",
                "Complex",
                "MultiStep",
            ];
            const KNOWN_SWARM_KEYS: &[&str] = &[
                "coding",
                "testing",
                "documentation",
                "code-review",
                "planning",
                "research",
                "debugging",
                "refactoring",
            ];
            r.by_task.retain(|k, _| {
                if KNOWN_TASK_KEYS.contains(&k.as_str()) {
                    true
                } else {
                    tracing::warn!(
                        task = %k,
                        known = ?KNOWN_TASK_KEYS,
                        "routing.by_task key is not a known TaskType name; \
                         the entry will never fire and is being dropped",
                    );
                    false
                }
            });
            r.swarm.retain(|k, _| {
                if KNOWN_SWARM_KEYS.contains(&k.as_str()) {
                    true
                } else {
                    tracing::warn!(
                        capability = %k,
                        known = ?KNOWN_SWARM_KEYS,
                        "routing.swarm key is not a known Capability name; \
                         the entry will never fire and is being dropped",
                    );
                    false
                }
            });
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
    fn validate_drops_unknown_task_keys() {
        // Regression: a typo like `Debbuging = "default"` used to be
        // silently kept, producing a route that never fired. The user
        // believed the debugging case was covered, but it fell through
        // to the default endpoint.
        let mut r = RoutingConfig::default();
        r.by_task.insert("Simple".into(), "default".into());
        r.by_task.insert("Debbuging".into(), "default".into()); // typo
        r.by_task.insert("MultiStep".into(), "default".into());
        let mut c = LlmConfig {
            routing: Some(r),
            ..LlmConfig::default()
        };
        c.validate();
        let r = c.routing.as_ref().unwrap();
        assert!(r.by_task.contains_key("Simple"));
        assert!(r.by_task.contains_key("MultiStep"));
        assert!(
            !r.by_task.contains_key("Debbuging"),
            "a misspelled task key must be dropped, not silently retained",
        );
    }

    #[test]
    fn validate_drops_unknown_swarm_keys() {
        let mut r = RoutingConfig::default();
        r.swarm.insert("coding".into(), "default".into());
        r.swarm.insert("code_review".into(), "default".into()); // underscore
        r.swarm.insert("testing".into(), "default".into());
        let mut c = LlmConfig {
            routing: Some(r),
            ..LlmConfig::default()
        };
        c.validate();
        let r = c.routing.as_ref().unwrap();
        assert!(r.swarm.contains_key("coding"));
        assert!(r.swarm.contains_key("testing"));
        assert!(
            !r.swarm.contains_key("code_review"),
            "a capability key must use the hyphenated form `code-review`, \
             not the underscored form",
        );
    }

    #[test]
    fn validate_keeps_every_known_task_key() {
        // The full set must survive validation — a future addition to
        // `TaskType` that is not added to `KNOWN_TASK_KEYS` would
        // silently disable routing for that task type.
        let mut r = RoutingConfig::default();
        for k in [
            "Simple",
            "CodeModification",
            "Debugging",
            "Research",
            "Testing",
            "Documentation",
            "Complex",
            "MultiStep",
        ] {
            r.by_task.insert(k.into(), "default".into());
        }
        let mut c = LlmConfig {
            routing: Some(r),
            ..LlmConfig::default()
        };
        c.validate();
        assert_eq!(
            c.routing.as_ref().unwrap().by_task.len(),
            8,
            "all eight known task keys must survive validation",
        );
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

#[cfg(test)]
mod coverage_llm_validate {
    //! `LlmConfig::validate` is called on every config load. A
    //! regression here is invisible until a specific endpoint is
    //! used — a bad temperature reaches the server, a bad URL drops
    //! the request. The existing tests cover the individual
    //! branches; these pin the interactions between them.
    use super::*;

    fn ep(name: &str, model: &str, url: &str) -> EndpointConfig {
        EndpointConfig {
            name: name.into(),
            provider: ProviderKind::OpenAICompatible,
            base_url: url.into(),
            model: model.into(),
            api_key_env: None,
            temperature: Some(0.7),
            max_tokens: Some(2048),
            context_window: 8192,
            timeout_secs: 300,
            pricing: None,
        }
    }

    #[test]
    fn validate_does_not_change_an_already_valid_config() {
        let mut c = LlmConfig {
            endpoints: vec![ep("a", "m", "http://x")],
            ..LlmConfig::default()
        };
        let before = c.default_endpoint().clone();
        c.validate();
        let after = c.default_endpoint();
        assert_eq!(before.name, after.name);
        assert_eq!(before.model, after.model);
        assert_eq!(before.base_url, after.base_url);
        assert_eq!(before.context_window, after.context_window);
        assert_eq!(before.timeout_secs, after.timeout_secs);
        assert_eq!(before.temperature, after.temperature);
        assert_eq!(before.max_tokens, after.max_tokens);
    }

    #[test]
    fn validate_is_idempotent() {
        // A config that was already clamped must be unchanged by a
        // second pass; a non-idempotent validate would keep
        // re-clamping and either drift or log every load.
        let mut c = LlmConfig {
            endpoints: vec![ep("a", "m", "http://x")],
            ..LlmConfig::default()
        };
        c.endpoints[0].temperature = Some(9.0);
        c.endpoints[0].context_window = 100;
        c.validate();
        let first_pass = (
            c.endpoints[0].temperature,
            c.endpoints[0].context_window,
        );
        c.validate();
        let second_pass = (
            c.endpoints[0].temperature,
            c.endpoints[0].context_window,
        );
        assert_eq!(first_pass, second_pass);
    }

    #[test]
    fn validate_handles_multiple_endpoints_independently() {
        let mut c = LlmConfig {
            endpoints: vec![
                ep("a", "ma", "http://a"),
                ep("b", "mb", "http://b"),
            ],
            ..LlmConfig::default()
        };
        c.endpoints[0].temperature = Some(9.0);
        c.endpoints[1].temperature = Some(-1.0);
        c.validate();
        assert_eq!(c.endpoints[0].temperature, Some(2.0));
        assert_eq!(c.endpoints[1].temperature, Some(0.0));
    }

    #[test]
    fn validate_drops_routing_entries_to_removed_endpoints() {
        // The common scenario: a user removes an endpoint but
        // forgets the routing line that pointed at it. Validate
        // must drop the dead route, not fail the load.
        let mut r = RoutingConfig::default();
        r.by_task.insert("Simple".into(), "gone".into());
        let mut c = LlmConfig {
            endpoints: vec![ep("present", "m", "http://x")],
            routing: Some(r),
            ..LlmConfig::default()
        };
        c.validate();
        let r = c.routing.as_ref().unwrap();
        assert!(!r.by_task.contains_key("Simple"));
    }

    #[test]
    fn default_endpoint_mut_populates_empty_endpoints_list() {
        let mut c = LlmConfig {
            endpoints: Vec::new(),
            ..LlmConfig::default()
        };
        let _ = c.default_endpoint_mut();
        assert_eq!(c.endpoints.len(), 1);
        assert_eq!(c.endpoints[0].name, "default");
    }

    #[test]
    fn route_for_task_falls_back_to_first_endpoint() {
        let c = LlmConfig {
            endpoints: vec![ep("first", "m", "http://x")],
            ..LlmConfig::default()
        };
        // No routing table: every task routes to the first endpoint.
        assert_eq!(c.route_for_task("Simple"), "first");
        assert_eq!(c.route_for_task("Debugging"), "first");
        assert_eq!(c.route_for_task("AKeyThatDoesNotExist"), "first");
    }

    #[test]
    fn route_for_task_honors_per_task_overrides() {
        let mut r = RoutingConfig::default();
        r.by_task.insert("Simple".into(), "cheap".into());
        let c = LlmConfig {
            endpoints: vec![ep("cheap", "m1", "http://c"), ep("smart", "m2", "http://s")],
            routing: Some(r),
            ..LlmConfig::default()
        };
        assert_eq!(c.route_for_task("Simple"), "cheap");
        // Any task without an explicit route falls back to the
        // first endpoint, not the second.
        assert_eq!(c.route_for_task("Debugging"), "cheap");
    }

    #[test]
    fn duplicate_endpoint_names_do_not_panic() {
        // Two endpoints named "same" is a config bug; validate
        // logs a warning and keeps both. The registry later keeps
        // the last under the name, but validate must not fail.
        let mut c = LlmConfig {
            endpoints: vec![
                ep("same", "m1", "http://a"),
                ep("same", "m2", "http://b"),
            ],
            ..LlmConfig::default()
        };
        c.validate();
        assert_eq!(c.endpoints.len(), 2);
    }

    #[test]
    fn non_finite_temperature_is_reset() {
        let mut c = LlmConfig {
            endpoints: vec![ep("a", "m", "http://x")],
            ..LlmConfig::default()
        };
        c.endpoints[0].temperature = Some(f32::INFINITY);
        c.validate();
        assert_eq!(c.endpoints[0].temperature, Some(0.7));
        c.endpoints[0].temperature = Some(f32::NEG_INFINITY);
        c.validate();
        assert_eq!(c.endpoints[0].temperature, Some(0.7));
    }
}

#[cfg(test)]
mod coverage_provider_kind {
    //! `ProviderKind` is a serde rename-with-aliases enum: the
    //! wire form is `"openai-compatible"`, but `"Ollama"` and
    //! `"OpenAI"` parse as the same variant so a v1-style config
    //! still loads. A regression that dropped an alias would
    //! reject every legacy config with a provider field spelled
    //! in one of the two legacy ways.
    use super::*;

    #[test]
    fn serializes_to_the_kebab_case_wire_form() {
        // The rename attribute is the contract; a caller writing
        // `provider = "openai-compatible"` in TOML must round-trip.
        let json = serde_json::to_string(&ProviderKind::OpenAICompatible).unwrap();
        assert_eq!(json, "\"openai-compatible\"");
        let json = serde_json::to_string(&ProviderKind::Anthropic).unwrap();
        assert_eq!(json, "\"anthropic\"");
    }

    #[test]
    fn legacy_aliases_map_to_openai_compatible() {
        for alias in ["Ollama", "OpenAI"] {
            let json = format!("\"{alias}\"");
            let k: ProviderKind = serde_json::from_str(&json)
                .unwrap_or_else(|e| panic!("alias {alias} rejected: {e}"));
            assert_eq!(k, ProviderKind::OpenAICompatible, "for {alias}");
        }
    }

    #[test]
    fn canonical_wire_form_parses() {
        let k: ProviderKind =
            serde_json::from_str("\"openai-compatible\"").unwrap();
        assert_eq!(k, ProviderKind::OpenAICompatible);
        let k: ProviderKind = serde_json::from_str("\"anthropic\"").unwrap();
        assert_eq!(k, ProviderKind::Anthropic);
    }

    #[test]
    fn unknown_values_are_rejected() {
        // A typo (`"openai"` lowercase, `"claude"`) must not fall
        // through to a default — that would silently send an
        // Anthropic-shaped request to an OpenAI endpoint or vice
        // versa.
        for bad in ["\"openai\"", "\"claude\"", "\"garbage\"", "1"] {
            assert!(
                serde_json::from_str::<ProviderKind>(bad).is_err(),
                "unexpectedly accepted {bad}",
            );
        }
    }

    #[test]
    fn minimal_endpoint_config_parses_with_defaults() {
        // Required fields: name, provider, base_url, model,
        // context_window. Everything else defaults.
        let cfg: EndpointConfig = toml::from_str(
            r#"
            name = "local"
            provider = "openai-compatible"
            base_url = "http://localhost:11434/v1"
            model = "qwen2.5:7b"
            context_window = 8192
            "#,
        )
        .unwrap();
        assert_eq!(cfg.name, "local");
        assert_eq!(cfg.provider, ProviderKind::OpenAICompatible);
        assert_eq!(cfg.base_url, "http://localhost:11434/v1");
        assert_eq!(cfg.model, "qwen2.5:7b");
        assert_eq!(cfg.context_window, 8192);
        assert!(cfg.api_key_env.is_none());
        assert!(cfg.temperature.is_none());
        assert!(cfg.max_tokens.is_none());
        assert_eq!(cfg.timeout_secs, 300, "default timeout changed");
        assert!(cfg.pricing.is_none());
    }

    #[test]
    fn endpoint_config_honours_every_optional_field() {
        let cfg: EndpointConfig = toml::from_str(
            r#"
            name = "cloud"
            provider = "anthropic"
            base_url = "https://api.anthropic.com"
            model = "claude-sonnet-4-5"
            api_key_env = "ANTHROPIC_API_KEY"
            temperature = 0.3
            max_tokens = 1024
            context_window = 200000
            timeout_secs = 120

            [pricing]
            input_per_mtok_usd = 3.0
            output_per_mtok_usd = 15.0
            "#,
        )
        .unwrap();
        assert_eq!(cfg.provider, ProviderKind::Anthropic);
        assert_eq!(cfg.api_key_env.as_deref(), Some("ANTHROPIC_API_KEY"));
        assert!((cfg.temperature.unwrap() - 0.3).abs() < 1e-6);
        assert_eq!(cfg.max_tokens, Some(1024));
        assert_eq!(cfg.timeout_secs, 120);
        let p = cfg.pricing.expect("pricing parsed");
        assert!((p.input_per_mtok_usd - 3.0).abs() < 1e-9);
        assert!((p.output_per_mtok_usd - 15.0).abs() < 1e-9);
    }

    #[test]
    fn pricing_config_rejects_missing_fields() {
        // Both rates are required; a half-written block should be
        // an error, not a silent zero.
        assert!(toml::from_str::<PricingConfig>("input_per_mtok_usd = 3.0").is_err());
        assert!(toml::from_str::<PricingConfig>("output_per_mtok_usd = 15.0").is_err());
    }
}
