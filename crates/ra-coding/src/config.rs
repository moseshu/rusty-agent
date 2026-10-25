//! Explicit product configuration values, provenance and composition primitives.
//!
//! These entries follow the Codex product configuration contract. They accept supplied values;
//! filesystem discovery, requirements composition and the effective layer stack are separate
//! consumers. The framework's generic six-source helper remains available independently.

mod domain;
mod fingerprint;
mod key_aliases;
mod mcp_types;
mod merge;
mod overrides;
mod source;
mod types;

pub use fingerprint::version_for_toml;
pub use mcp_types::{AppToolApproval, McpServerToolConfig};
pub use merge::{
    ShellEnvironmentPolicyFilterRepresentation, is_structured_feature_path, merge_toml_values,
    shell_environment_filter_entry,
};
pub use overrides::build_cli_overrides_layer;
pub use source::{ConfigLayer, ConfigLayerMetadata, ConfigLayerSource, format_config_layer_source};

pub use types::{
    MarketplaceConfig, MarketplaceSourceType, PluginConfig, PluginMcpServerConfig,
    PluginsConfigToml, PluginsDefaultConfig,
};
