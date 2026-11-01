#![doc = include_str!("../README.md")]
#![warn(missing_docs)]
/// The config config module.
pub mod config;
/// The instructions config module.
pub mod instructions;
/// The jev config module.
pub mod jev;
/// The limits config module.
pub mod limits;
/// The llm config module.
pub mod llm;
/// The mcp config module.
pub mod mcp;
/// The memory config module.
pub mod memory;
/// The policy config module.
pub mod policy;
/// The profiles config module.
pub mod profiles;
/// The retry config module.
pub mod retry;
/// The skills config module.
pub mod skills;
/// The swarm config module.
pub mod swarm;

pub use config::{HooksConfig, KodConfig, LspConfig, RedactConfig, SecurityConfig, ToolsConfig};
pub use jev::{JevConfig, JevThresholds};
pub use limits::{LimitsConfig, OnExhausted, ToolQuota};
pub use llm::{
    DEFAULT_RATE_LIMIT_WAIT_SECS, EndpointConfig, LlmConfig, PricingConfig, ProviderKind,
    RoutingConfig,
};
pub use mcp::{McpConfig, McpServerConfig};
pub use memory::{EmbeddingEndpoint, MemoryConfig, MemoryScope};
pub use policy::{
    Decision, GitPolicy, Policy, PolicyDecision, PolicyEngine, PolicySource, Preset, ReadMode,
    ReadProtection, SessionDeny, ToolPolicy,
};
pub use retry::RetryConfig;
pub use skills::SkillsConfig;
pub use swarm::{Isolation, SwarmConfig};
