//! LLM provider configuration.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LlmConfig {
    // ---- v1 legacy fields ----
    //
    // Kept in the struct so existing config.toml files keep parsing.
    // `effective_endpoints()` synthesises a single `"default"` endpoint
    // from these when `endpoints` is empty, and every consumer that
    // used to read `llm.model` / `llm.base_url` should switch to that
    // synthesis path. The fields stay until every caller has migrated
    // (a deprecation cycle, not a removal).
    pub provider: ProviderType,
    pub model: String,
    pub base_url: String,
    pub api_key: Option<String>,
    pub context_window: usize,
    pub max_tokens: usize,
    pub temperature: f32,
    pub timeout_secs: u64,

    // ---- v2 fields ----
    //
    // An empty `endpoints` vector is the v1 shape; a non-empty one
    // activates the v2 path. `routing` is v2-only and is `None` in v1.
    #[serde(default)]
    pub endpoints: Vec<EndpointConfig>,
    #[serde(default)]
    pub routing: Option<RoutingConfig>,
    /// Allow the agent to reach the network through `web_fetch` (and,
    /// once a search tool exists, that too). Default false: the agent
    /// running with a user's shell privileges should not reach out
    /// without an explicit opt-in. A local-only session with a local
    /// model gains nothing from it; a session that does need docs at a
    /// URL turns it on.
    pub network_access: bool,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            provider: ProviderType::OpenAICompatible,
            model: "codellama:13b".to_string(),
            base_url: "http://localhost:11434/v1".to_string(),
            api_key: None,
            context_window: 8192,
            max_tokens: 2048,
            temperature: 0.7,
            timeout_secs: 300,
            network_access: false,
            endpoints: Vec::new(),
            routing: None,
        }
    }
}

impl LlmConfig {
    /// Clamp out-of-range or nonsensical values to safe bounds, emitting
    /// a `tracing::warn!` for each one changed. Called by
    /// `KodConfig::load_default` after deserialization.
    ///
    /// This is intentionally a soft validation: a typo like
    /// `temperature = 2.5` (a user meaning 0.25 or 25% depending on
    /// their convention) currently goes straight to the provider, which
    /// either clamps silently or rejects with an opaque HTTP 400. A
    /// `context_window = 0` reaches `render_history`'s char caps and
    /// the TUI's meter unclamped. Clamping here means the session
    /// still runs, the value in effect is one the user can see in the
    /// warning, and the next prompt has a chance to work.
    ///
    /// Does not change `LlmConfig::default()` — the defaults are already
    /// in range. Only user-supplied configs are affected.
    pub fn validate(&mut self) {
        // Temperature: providers accept roughly 0.0–2.0 (OpenAI's range).
        // A negative value is always wrong; anything above 2.0 is either
        // a typo for a percentage or an attempt the server will reject.
        let t = self.temperature;
        if !t.is_finite() {
            tracing::warn!(
                value = %t,
                "llm.temperature is not finite; falling back to 0.7"
            );
            self.temperature = 0.7;
        } else if t < 0.0 {
            tracing::warn!(
                value = %t,
                "llm.temperature is negative; clamping to 0.0"
            );
            self.temperature = 0.0;
        } else if t > 2.0 {
            tracing::warn!(
                value = %t,
                "llm.temperature > 2.0 is not accepted by most providers; clamping to 2.0"
            );
            self.temperature = 2.0;
        }

        // Context window. Three downstream floors have to be
        // consistent with this value, or the session behaves
        // incoherently:
        //
        //   - `KodApp::set_context_limit` floors the meter at 1000
        //     tokens.
        //   - `KodEngine::set_history_budget` floors the rendered
        //     history at `MIN_HISTORY_CHAR_BUDGET = 4000` chars
        //     (~1000 tokens at 4 chars/token), and the TUI passes
        //     `context_window * 3`.
        //   - `KodApp::maybe_compact` fires when the accumulated
        //     token estimate exceeds 4/5 of the (floored) window.
        //
        // A user who writes `context_window = 500` gets a meter that
        // says 1000, a history budget that clamps to 4000 chars, and
        // an auto-compact threshold based on 1000 — three numbers that
        // all pretend the window is bigger than the user set, and the
        // history budget alone already overshoots the window. The
        // session works but its accounting lies.
        //
        // Clamp to a coherent minimum instead of letting the incoherent
        // case run. The floor is 2000 tokens: high enough that
        // `2000 * 3 = 6000` chars of history clears the engine's 4000
        // char floor with headroom, and the meter's 4/5 threshold
        // (1600 tokens) leaves room for the prompt scaffolding and a
        // tool result or two before compaction fires.
        //
        // 0 is treated separately: a config file that predates the
        // field, or a user who wrote `context_window = 0` meaning
        // "unknown, use the default", gets `LlmConfig::default()`'s
        // 8192 rather than the floor.
        const MIN_CONTEXT_WINDOW: usize = 2_000;
        if self.context_window == 0 {
            tracing::warn!(
                "llm.context_window is 0; falling back to {}",
                LlmConfig::default().context_window
            );
            self.context_window = LlmConfig::default().context_window;
        } else if self.context_window < MIN_CONTEXT_WINDOW {
            tracing::warn!(
                value = self.context_window,
                floor = MIN_CONTEXT_WINDOW,
                "llm.context_window is below the coherent minimum; clamping to {}",
                MIN_CONTEXT_WINDOW
            );
            self.context_window = MIN_CONTEXT_WINDOW;
        }

        // max_tokens == 0 is worse than context_window == 0 — some
        // providers interpret it as "generate nothing", which returns
        // an empty reply that looks like a silent failure. Clamp to the
        // default.
        if self.max_tokens == 0 {
            tracing::warn!(
                "llm.max_tokens is 0; falling back to {}",
                LlmConfig::default().max_tokens
            );
            self.max_tokens = LlmConfig::default().max_tokens;
        }

        // A tiny timeout_secs makes even a cold Ollama start fail before
        // the model finishes loading. 1s is the floor below which the
        // timeout is almost certainly a mistake.
        if self.timeout_secs < 1 {
            tracing::warn!(
                value = self.timeout_secs,
                "llm.timeout_secs < 1; clamping to 1"
            );
            self.timeout_secs = 1;
        }

        // Base URL: an empty string would make every request fail with
        // a confusing DNS error. Fall back to the default and warn.
        if self.base_url.trim().is_empty() {
            tracing::warn!(
                "llm.base_url is empty; falling back to {}",
                LlmConfig::default().base_url
            );
            self.base_url = LlmConfig::default().base_url;
        }

        // Model: empty model name is rejected by every provider.
        if self.model.trim().is_empty() {
            tracing::warn!(
                "llm.model is empty; falling back to {}",
                LlmConfig::default().model
            );
            self.model = LlmConfig::default().model;
        }

        // Provider: only OpenAICompatible is implemented today. The
        // other variants are recognised so a config with them loads
        // (the user can still see it via `kod config`), but the CLI
        // constructs an OpenAI-compatible client regardless, so an
        // Anthropic or Custom config fails at the first prompt with
        // whatever error the server returns. Warn loudly here so the
        // mismatch is named at startup rather than discovered after a
        // wasted prompt.
        match self.provider {
            ProviderType::OpenAICompatible => {}
            ProviderType::Anthropic | ProviderType::Custom => {
                tracing::warn!(
                    provider = ?self.provider,
                    "llm.provider names a protocol that kod does not yet speak; \
                     the CLI will send OpenAI-compatible requests to {} and the \
                     server is likely to reject them. Set provider = \"OpenAICompatible\" \
                     for now (Anthropic and Custom support is planned).",
                    self.base_url
                );
            }
        }
    }
    /// The effective endpoint list after applying the v1 -> v2
    /// migration. Three cases:
    ///
    /// - **v2 explicit**: `endpoints` is non-empty. Returned as-is
    ///   after validation.
    /// - **v1 only** (`endpoints` empty): synthesise exactly one
    ///   endpoint named `"default"` from the legacy `provider` /
    ///   `model` / `base_url` / etc. fields. This is what every
    ///   caller that read `llm.model` directly was doing, without
    ///   going through a synthesis step. The result is deterministic
    ///   and diffable, which matters for `/debug last-prompt`.
    /// - **v1 fallback** (`provider == Anthropic` or `Custom`): the
    ///   legacy enum has variants that have no v2 equivalent today.
    ///   The synthesis emits an OpenAICompatible endpoint and logs a
    ///   warning (same spirit as `validate()`, at the synthesis
    ///   layer where the caller actually acts on the result).
    ///
    /// Also validates: endpoint names unique; every route points at
    /// an existing endpoint; at least one endpoint present. Returns
    /// `Err(KodError::Config)` on failure so a typo fails startup
    /// loudly rather than at the first prompt.
    pub fn effective_endpoints(&self) -> kod_error::Result<Vec<EndpointConfig>> {
        let endpoints = if self.endpoints.is_empty() {
            let provider = match self.provider {
                ProviderType::Anthropic | ProviderType::Custom => {
                    tracing::warn!(
                        provider = ?self.provider,
                        "v1 config uses an unsupported provider; the synthesised \
                         endpoint will speak the OpenAI-compatible protocol"
                    );
                    ProviderKind::OpenAICompatible
                }
                ProviderType::OpenAICompatible => ProviderKind::OpenAICompatible,
            };
            vec![EndpointConfig {
                name: "default".to_string(),
                provider,
                base_url: self.base_url.clone(),
                model: self.model.clone(),
                api_key_env: None,
                temperature: Some(self.temperature),
                max_tokens: Some(self.max_tokens),
                context_window: self.context_window,
                timeout_secs: self.timeout_secs,
                pricing: None,
            }]
        } else {
            self.endpoints.clone()
        };

        // Validate uniqueness.
        let mut seen = std::collections::HashSet::new();
        for e in &endpoints {
            if e.name.trim().is_empty() {
                return Err(kod_error::KodError::Config(
                    "llm.endpoints: endpoint name must not be empty".to_string(),
                ));
            }
            if !seen.insert(e.name.clone()) {
                return Err(kod_error::KodError::Config(format!(
                    "llm.endpoints: duplicate endpoint name {:?}",
                    e.name
                )));
            }
        }

        // Validate routing references.
        if let Some(routing) = &self.routing {
            let names: std::collections::HashSet<&str> =
                endpoints.iter().map(|e| e.name.as_str()).collect();
            for (task, target) in &routing.by_task {
                if !names.contains(target.as_str()) {
                    return Err(kod_error::KodError::Config(format!(
                        "llm.routing.by_task[{}]: unknown endpoint {:?}. Known: {:?}",
                        task,
                        target,
                        names
                    )));
                }
            }
            for target in &routing.fallback {
                if !names.contains(target.as_str()) {
                    return Err(kod_error::KodError::Config(format!(
                        "llm.routing.fallback: unknown endpoint {:?}. Known: {:?}",
                        target, names
                    )));
                }
            }
            for (cap, target) in &routing.swarm {
                if !names.contains(target.as_str()) {
                    return Err(kod_error::KodError::Config(format!(
                        "llm.routing.swarm[{}]: unknown endpoint {:?}. Known: {:?}",
                        cap, target, names
                    )));
                }
            }
        }

        Ok(endpoints)
    }

    /// The endpoint a task type routes to, honouring the routing
    /// table when present and falling back to the single "default"
    /// endpoint otherwise. `task_key` is the string form of a
    /// `TaskType` (`"Simple"`, `"Debugging"`, ...); the engine passes
    /// it rather than the enum so kod-config stays free of a
    /// kod-core dependency.
    pub fn route_for_task(&self, task_key: &str) -> String {
        if let Some(routing) = &self.routing
            && let Some(target) = routing.by_task.get(task_key)
        {
            return target.clone();
        }
        // v1 synthesis or an unrouted task: the first endpoint is the
        // sensible default. `effective_endpoints` guarantees at least
        // one exists (via synthesis).
        "default".to_string()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ProviderType {
    /// Any OpenAI-spec chat-completions endpoint: Ollama `/v1`, LM Studio,
    /// MLX Omni Serve, vLLM, and OpenAI itself — they all speak the same
    /// wire protocol, so one code path serves them all. `Ollama` and
    /// `OpenAI` are accepted as aliases so config files written against
    /// earlier enum names keep loading.
    #[serde(alias = "Ollama", alias = "OpenAI")]
    OpenAICompatible,
    /// Anthropic's Messages API. Not implemented — a config with this
    /// provider loads (so `kod config` shows it) but the first prompt
    /// will fail. `LlmConfig::validate` warns about this at startup.
    Anthropic,
    /// Anything else. Same situation as Anthropic: recognised as a
    /// provider name, not implemented.
    Custom,
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
    fn test_default_llm_config() {
        let config = LlmConfig::default();
        assert_eq!(config.provider, ProviderType::OpenAICompatible);
        assert_eq!(config.model, "codellama:13b");
        assert_eq!(config.base_url, "http://localhost:11434/v1");
    }

    #[test]
    fn test_validate_clamps_temperature() {
        let mut c = LlmConfig {
            temperature: 5.0,
            ..LlmConfig::default()
        };
        c.validate();
        assert_eq!(c.temperature, 2.0);

        let mut c = LlmConfig {
            temperature: -1.0,
            ..LlmConfig::default()
        };
        c.validate();
        assert_eq!(c.temperature, 0.0);

        let mut c = LlmConfig {
            temperature: f32::NAN,
            ..LlmConfig::default()
        };
        c.validate();
        assert_eq!(c.temperature, 0.7);

        // A valid value is untouched.
        let mut c = LlmConfig {
            temperature: 0.3,
            ..LlmConfig::default()
        };
        c.validate();
        assert_eq!(c.temperature, 0.3);
    }

    #[test]
    fn test_validate_clamps_zero_context_and_max_tokens() {
        let mut c = LlmConfig {
            context_window: 0,
            max_tokens: 0,
            ..LlmConfig::default()
        };
        c.validate();
        assert_eq!(c.context_window, LlmConfig::default().context_window);
        assert_eq!(c.max_tokens, LlmConfig::default().max_tokens);
    }

    /// A `context_window` below the coherent floor must be clamped up,
    /// not passed through. Regression: a window of 500 produced an
    /// incoherent session where the meter's floor (1000), the history
    /// budget's floor (4000 chars ≈ 1000 tokens), and the auto-compact
    /// threshold (4/5 of the floored window) all disagreed about how
    /// much room the model had.
    #[test]
    fn test_validate_clamps_tiny_context_window() {
        let mut c = LlmConfig {
            context_window: 500,
            ..LlmConfig::default()
        };
        c.validate();
        assert_eq!(
            c.context_window, 2_000,
            "tiny window must clamp to the coherent floor"
        );

        // Boundary: 1999 clamps, 2000 does not.
        let mut c = LlmConfig {
            context_window: 1_999,
            ..LlmConfig::default()
        };
        c.validate();
        assert_eq!(c.context_window, 2_000);

        let mut c = LlmConfig {
            context_window: 2_000,
            ..LlmConfig::default()
        };
        c.validate();
        assert_eq!(c.context_window, 2_000);

        // Above the floor is honored.
        let mut c = LlmConfig {
            context_window: 131_072,
            ..LlmConfig::default()
        };
        c.validate();
        assert_eq!(c.context_window, 131_072);

        // 0 is a special case that maps to the default, not the floor.
        let mut c = LlmConfig {
            context_window: 0,
            ..LlmConfig::default()
        };
        c.validate();
        assert_eq!(c.context_window, LlmConfig::default().context_window);
    }

    #[test]
    fn test_validate_floors_timeout() {
        let mut c = LlmConfig {
            timeout_secs: 0,
            ..LlmConfig::default()
        };
        c.validate();
        assert_eq!(c.timeout_secs, 1);
    }

    #[test]
    fn test_validate_falls_back_on_empty_strings() {
        let mut c = LlmConfig {
            base_url: "   ".to_string(),
            model: "".to_string(),
            ..LlmConfig::default()
        };
        c.validate();
        assert_eq!(c.base_url, LlmConfig::default().base_url);
        assert_eq!(c.model, LlmConfig::default().model);
    }

    #[test]
    fn test_validate_leaves_defaults_alone() {
        // The default config must be fully valid so a first-run
        // generate-defaults session behaves identically after validate.
        let mut c = LlmConfig::default();
        let expected = LlmConfig::default();
        c.validate();
        assert_eq!(c.temperature, expected.temperature);
        assert_eq!(c.context_window, expected.context_window);
        assert_eq!(c.max_tokens, expected.max_tokens);
        assert_eq!(c.timeout_secs, expected.timeout_secs);
        assert_eq!(c.base_url, expected.base_url);
        assert_eq!(c.model, expected.model);
    }

    /// `provider = "OpenAI"` must still deserialize — the OpenAI API
    /// IS the OpenAI-compatible protocol, so the two variants were
    /// merged. A config written against the old enum value must keep
    /// loading.
    #[test]
    fn test_legacy_openai_provider_alias() {
        let config: LlmConfig = toml::from_str(
            r#"
            provider = "OpenAI"
            model = "gpt-4o-mini"
            base_url = "https://api.openai.com"
            context_window = 128000
            max_tokens = 4096
            temperature = 0.7
            timeout_secs = 120
            "#,
        )
        .unwrap();
        assert_eq!(config.provider, ProviderType::OpenAICompatible);
    }

    /// Non-OpenAICompatible providers must not silently pass through
    /// validate(); the warn! cannot be asserted directly, but the
    /// variant must survive validate() unchanged (no accidental
    /// normalization), and OpenAICompatible must be a no-op.
    #[test]
    fn test_validate_leaves_provider_choice_intact() {
        // Anthropic is unsupported but loadable. validate must not
        // rewrite it — the warning is the entire user-facing signal,
        // and changing the enum behind the user's back would be worse
        // than the warning.
        let mut c = LlmConfig {
            provider: ProviderType::Anthropic,
            ..LlmConfig::default()
        };
        c.validate();
        assert_eq!(c.provider, ProviderType::Anthropic);

        let mut c = LlmConfig {
            provider: ProviderType::Custom,
            ..LlmConfig::default()
        };
        c.validate();
        assert_eq!(c.provider, ProviderType::Custom);

        let mut c = LlmConfig::default();
        c.validate();
        assert_eq!(c.provider, ProviderType::OpenAICompatible);
    }

    #[test]
    fn test_effective_endpoints_v1_synthesis() {
        // v1 config: no `endpoints`, no `routing`.
        let cfg: LlmConfig = toml::from_str(
            r#"
            provider = "OpenAICompatible"
            model = "qwen2.5-coder:7b"
            base_url = "http://localhost:11434/v1"
            context_window = 32768
            max_tokens = 4096
            temperature = 0.2
            timeout_secs = 120
            "#,
        )
        .unwrap();

        let eps = cfg.effective_endpoints().unwrap();
        assert_eq!(eps.len(), 1, "v1 should synthesise exactly one endpoint");
        let e = &eps[0];
        assert_eq!(e.name, "default");
        assert_eq!(e.provider, ProviderKind::OpenAICompatible);
        assert_eq!(e.model, "qwen2.5-coder:7b");
        assert_eq!(e.base_url, "http://localhost:11434/v1");
        assert_eq!(e.context_window, 32768);
        assert_eq!(e.max_tokens, Some(4096));
        assert_eq!(e.temperature, Some(0.2));
        assert_eq!(e.timeout_secs, 120);

        assert_eq!(cfg.route_for_task("Simple"), "default");
    }

    #[test]
    fn test_effective_endpoints_v2_explicit() {
        let cfg: LlmConfig = toml::from_str(
            r#"
            provider = "OpenAICompatible"
            model = "ignored"
            base_url = "http://localhost:11434/v1"
            context_window = 8192
            max_tokens = 2048
            temperature = 0.7
            timeout_secs = 300

            [[endpoints]]
            name = "local-ollama"
            provider = "openai-compatible"
            base_url = "http://localhost:11434/v1"
            model = "qwen2.5-coder:32b"
            context_window = 32768
            max_tokens = 4096

            [[endpoints]]
            name = "anthropic"
            provider = "anthropic"
            base_url = "https://api.anthropic.com"
            model = "claude-sonnet-4-5"
            context_window = 200000
            api_key_env = "ANTHROPIC_API_KEY"
            "#,
        )
        .unwrap();

        let eps = cfg.effective_endpoints().unwrap();
        assert_eq!(eps.len(), 2);
        assert_eq!(eps[0].name, "local-ollama");
        assert_eq!(eps[0].provider, ProviderKind::OpenAICompatible);
        assert_eq!(eps[1].name, "anthropic");
        assert_eq!(eps[1].provider, ProviderKind::Anthropic);
        assert_eq!(eps[1].api_key_env.as_deref(), Some("ANTHROPIC_API_KEY"));
    }

    #[test]
    fn test_effective_endpoints_rejects_duplicate_names() {
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
            name = "a"
            provider = "openai-compatible"
            base_url = "http://localhost:11434/v1"
            model = "m"
            context_window = 8192

            [[endpoints]]
            name = "a"
            provider = "openai-compatible"
            base_url = "http://localhost:11434/v1"
            model = "m"
            context_window = 8192
            "#,
        )
        .unwrap();

        // context_window missing -> default 0? No: it's a required
        // field on EndpointConfig. If the parse succeeded, context_window
        // defaults to 0 (no serde default) — which is fine for this test
        // since we only care about the name collision.
        let err = cfg.effective_endpoints().unwrap_err();
        assert!(
            err.to_string().contains("duplicate endpoint name"),
            "got: {err}"
        );
    }

    #[test]
    fn test_effective_endpoints_rejects_unknown_route_target() {
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
            name = "primary"
            provider = "openai-compatible"
            base_url = "http://localhost:11434/v1"
            model = "m"
            context_window = 8192

            [routing]
            fallback = ["nonexistent"]
            "#,
        )
        .unwrap();

        let err = cfg.effective_endpoints().unwrap_err();
        assert!(
            err.to_string().contains("unknown endpoint"),
            "got: {err}"
        );
    }

    #[test]
    fn test_route_for_task_uses_routing_table() {
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
            name = "local"
            provider = "openai-compatible"
            base_url = "http://localhost:11434/v1"
            model = "m"
            context_window = 8192

            [[endpoints]]
            name = "cloud"
            provider = "anthropic"
            base_url = "https://api.anthropic.com"
            model = "claude"
            context_window = 200000

            [routing.by_task]
            Simple = "local"
            Debugging = "cloud"
            "#,
        )
        .unwrap();

        assert_eq!(cfg.route_for_task("Simple"), "local");
        assert_eq!(cfg.route_for_task("Debugging"), "cloud");
        // Unrouted task falls back to "default".
        assert_eq!(cfg.route_for_task("Research"), "default");
    }

    #[test]
    fn test_legacy_ollama_provider_alias() {
        // Config files written before the rename still load.
        let config: LlmConfig = toml::from_str(
            r#"
            provider = "Ollama"
            model = "llama3.1"
            base_url = "http://localhost:11434"
            context_window = 8192
            max_tokens = 2048
            temperature = 0.7
            timeout_secs = 300
            "#,
        )
        .unwrap();
        assert_eq!(config.provider, ProviderType::OpenAICompatible);
    }
}
