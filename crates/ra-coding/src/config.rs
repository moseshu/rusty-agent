//! Compatibility exports from the shared configuration owner.
#[cfg(feature = "test-api")]
#[doc(hidden)]
pub use ra_config::mcp_requirements_test_api;
pub use ra_config::{
    AppToolApproval, ConfigLayer, ConfigLayerMetadata, ConfigLayerSource, Constrained,
    ConstrainedWithSource, ConstraintError, ConstraintResult, DEFAULT_MCP_SERVER_ENVIRONMENT_ID,
    EnvironmentMcpPolicy, HookEventsToml, HookHandlerConfig, HookStateToml, HooksFile, HooksToml,
    ManagedHooksRequirementsToml, MarketplaceConfig, MarketplaceSourceType, MatcherGroup,
    McpEmaRegistration, McpEnterpriseManagedAuthConfig, McpServerAuth, McpServerCommandMatcher,
    McpServerConfig, McpServerDisabledReason, McpServerEnvVar, McpServerIdentity,
    McpServerIdpOAuthConfig, McpServerOAuthConfig, McpServerRequirement, McpServerToolConfig,
    McpServerTransportConfig, McpServerValueMatcher, McpStartupReadiness, PluginConfig,
    PluginMcpRequirements, PluginMcpServerConfig, PluginsConfigToml, PluginsDefaultConfig,
    RawMcpServerConfig, RedactedString, RequirementSource, ScopedSkillsConfig,
    ShellEnvironmentPolicyFilterRepresentation, Sourced, ToolExposureSurface,
    build_cli_overrides_layer, format_config_layer_source, is_structured_feature_path,
    merge_toml_values, shell_environment_filter_entry, validate_mcp_server_requirement,
    version_for_toml,
};

pub use ra_config::{
    CONFIG_TOML_FILE, MarketplaceConfigUpdate, PluginConfigEdit, RemoveMarketplaceConfigOutcome,
    apply_user_plugin_config_edits, clear_user_plugin, record_user_marketplace,
    remove_user_marketplace, remove_user_marketplace_config, set_user_plugin_enabled,
};
