//! Explicit product configuration values, provenance and composition primitives.
//!
//! These entries follow the Codex product configuration contract. They accept supplied values;
//! filesystem discovery, requirements composition and the effective layer stack are separate
//! consumers. The framework's generic six-source helper remains available independently.

mod constraint;
mod domain;
mod fingerprint;
mod hook_config;
mod key_aliases;
mod mcp_ema;
mod mcp_requirements;
mod mcp_types;
mod merge;
mod overrides;
mod redacted_string;
mod requirement_source;
mod source;
mod sourced;
mod tool_exposure;
mod types;

pub use fingerprint::version_for_toml;
pub use hook_config::{
    HookEventsToml, HookHandlerConfig, HookStateToml, HooksFile, HooksToml,
    ManagedHooksRequirementsToml, MatcherGroup,
};
pub use mcp_ema::{McpEmaRegistration, McpEnterpriseManagedAuthConfig, McpServerIdpOAuthConfig};
pub use mcp_types::{
    AppToolApproval, DEFAULT_MCP_SERVER_ENVIRONMENT_ID, McpServerAuth, McpServerConfig,
    McpServerDisabledReason, McpServerEnvVar, McpServerOAuthConfig, McpServerToolConfig,
    McpServerTransportConfig, McpStartupReadiness, RawMcpServerConfig,
};
pub use merge::{
    ShellEnvironmentPolicyFilterRepresentation, is_structured_feature_path, merge_toml_values,
    shell_environment_filter_entry,
};
pub use overrides::build_cli_overrides_layer;
pub use redacted_string::RedactedString;
pub use requirement_source::RequirementSource;
pub use source::{ConfigLayer, ConfigLayerMetadata, ConfigLayerSource, format_config_layer_source};

pub use types::{
    MarketplaceConfig, MarketplaceSourceType, PluginConfig, PluginMcpServerConfig,
    PluginsConfigToml, PluginsDefaultConfig,
};

pub use tool_exposure::ToolExposureSurface;

pub use constraint::{Constrained, ConstraintError, ConstraintResult};
/// Internal matcher access for ported integration tests.
#[cfg(feature = "test-api")]
#[doc(hidden)]
pub use mcp_requirements::test_api as mcp_requirements_test_api;
pub use mcp_requirements::validate_mcp_server_requirement;
pub use ra_protocol::server::protocol::{
    McpServerCommandMatcher, McpServerIdentity, McpServerRequirement, McpServerValueMatcher,
    PluginMcpRequirements,
};
pub use sourced::{ConstrainedWithSource, Sourced};
