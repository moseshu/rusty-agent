//! Shared product configuration values, provenance and composition primitives.
//!
//! **Stability**: `Internal`. This crate owns the ported configuration contract shared by
//! product assembly and execution environments.
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
pub mod types;

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
mod mcp_policy;
pub use mcp_policy::{
    EnvironmentMcpPolicy, McpServerCommandMatcher, McpServerIdentity, McpServerRequirement,
    McpServerValueMatcher, PluginMcpRequirements,
};
pub use sourced::{ConstrainedWithSource, Sourced};

mod scoped_skills;
pub use scoped_skills::ScopedSkillsConfig;

/// Base-user product configuration filename.
pub const CONFIG_TOML_FILE: &str = "config.toml";
mod marketplace_edit;
mod plugin_edit;
pub use marketplace_edit::{
    MarketplaceConfigUpdate, RemoveMarketplaceConfigOutcome, record_user_marketplace,
    remove_user_marketplace, remove_user_marketplace_config,
};
pub use plugin_edit::{
    PluginConfigEdit, apply_user_plugin_config_edits, clear_user_plugin, set_user_plugin_enabled,
};

mod layer;
mod path_context;
mod policy;
mod profile_name;
mod web_search;
pub use layer::ConfigLayerEntry;
pub use path_context::ConfigPathContext;
pub use policy::{ApprovalsReviewer, AskForApproval, NetworkAccess, SandboxMode, SandboxPolicy};
pub use profile_name::{ProfileV2Name, ProfileV2NameParseError};
pub use web_search::WebSearchMode;

pub mod model_provider;
mod provider_auth;
mod provider_capabilities;
mod provider_gateway_oauth;
mod reasoning;
pub use model_provider::{
    AwsAuthRefreshConfig, AwsCredentialExportConfig, ModelProviderAwsAuthInfo, ModelProviderInfo,
    WireApi,
};
pub use provider_auth::ModelProviderAuthInfo;
pub use provider_capabilities::{ModelProviderCapabilities, RemoteCompactionSupport};
pub use provider_gateway_oauth::{GatewayOAuthConfig, GatewayOAuthDelivery};
pub use reasoning::ReasoningEffort;

pub mod features;
mod network_values;
pub use network_values::{CredentialAuthMethod, CredentialProviderConfig};
mod permissions_toml;
pub use features::{FeatureConfig, FeatureToml, FeaturesToml};
pub use permissions_toml::{
    FilesystemPermissionToml, FilesystemPermissionsToml, NetworkToml, PermissionProfileToml,
    PermissionsToml, WorkspaceRootsToml,
};

mod scalar_values;
mod skills_config;
mod tui_effects;
mod tui_keymap;
mod tui_mouse_scroll;
mod tui_rendering;
pub use features::{
    FEATURES, Feature, FeatureConfigSource, FeatureOverrides, Features, legacy_feature_keys,
};
pub use scalar_values::{
    AltScreenMode, AsyncClassifierMode, AutoCompactTokenLimitScope, MemoryVersion, ModeKind,
    Personality, RealtimeConversationVersion, RealtimeVoice, ReasoningSummary,
    SERVICE_TIER_DEFAULT_REQUEST_VALUE, ServiceTier, TUI_VISIBLE_COLLABORATION_MODES,
    ToolResultLogConfig, TranscriptFormat, TrustLevel, Verbosity, WebSearchConfig,
    WebSearchContextSize, WebSearchFilters, WebSearchLocation, WebSearchToolConfig,
    WebSearchUserLocation, WebSearchUserLocationType, WindowsSandboxLevel,
    WindowsSandboxProxySettingsMode,
};
pub use skills_config::{
    BundledSkillsConfig, SkillConfig, SkillConfigRule, SkillConfigRuleSelector, SkillConfigRules,
    SkillsConfig, bundled_skills_enabled_from_stack, skill_config_rules_from_stack,
};

mod browser_computer_use_requirements;
mod browser_use;
mod computer_use;
pub mod config_toml;
mod granular_approval;
mod profile_toml;
mod project_trust;
pub mod schema;
mod shell_environment_policy;
mod shell_values;
pub use browser_computer_use_requirements::{
    AllowDenyRequirementToml, BrowserUseAccessApprovalLifetimeToml,
    BrowserUseExtensionRequirementsToml, BrowserUseOriginPolicyToml, BrowserUseRequirementsToml,
    ComputerUseMacosRequirementsToml, ComputerUseRequirementsToml,
    ComputerUseWindowsExeRequirementToml, ComputerUseWindowsRequirementsToml, RequestHeaderToml,
};
pub use config_toml::{ConfigToml, ProjectConfig};
pub use granular_approval::GranularApprovalConfig;
pub use project_trust::{ProjectTrustLookup, ProjectTrustPath};
pub use types::{
    AgentsOverviewGrouping, AnalyticsConfigToml, AppConfig, AppLinkConfig, AppLinksConfig,
    AppToolConfig, AppToolsConfig, AppsConfigToml, AppsDefaultConfig, CopyOnSelect,
    DEFAULT_MEMORIES_MAX_RAW_MEMORIES_FOR_CONSOLIDATION, DEFAULT_MEMORIES_MAX_ROLLOUT_AGE_DAYS,
    DEFAULT_MEMORIES_MAX_ROLLOUTS_PER_STARTUP, DEFAULT_MEMORIES_MAX_UNUSED_DAYS,
    DEFAULT_MEMORIES_MIN_RATE_LIMIT_REMAINING_PERCENT, DEFAULT_MEMORIES_MIN_ROLLOUT_IDLE_HOURS,
    DEFAULT_OTEL_ENVIRONMENT, DEFAULT_TERMINAL_RESIZE_REFLOW_FALLBACK_MAX_ROWS,
    ExternalConfigMigrationPrompts, FeedbackConfigToml, History, HistoryPersistence,
    MemoriesConfig, MemoriesToml, ModelAvailabilityNuxConfig, Notice, NotificationCondition,
    NotificationMethod, Notifications, OAuthCredentialsStoreMode, OtelConfig, OtelConfigToml,
    OtelExporterKind, OtelHttpProtocol, OtelTlsConfig, ResumeCwdMode, RightClickPaste,
    SandboxWorkspaceWrite, SessionPickerViewMode, ToolSuggestConfig, ToolSuggestDisabledTool,
    ToolSuggestDiscoverable, ToolSuggestDiscoverableType, Tui, TuiNotificationSettings,
    TuiPetAnchor, UriBasedFileOpener, WindowsSandboxModeToml, WindowsToml,
};

mod application_requirements;
mod config_requirements;
mod in_app_browser_requirements;
mod requirements_exec_policy;
pub use application_requirements::{
    ApplicationNetworkRequirementsToml, ApplicationRequirementsToml,
};
pub use config_requirements::{
    AppRequirementToml, AppToolRequirementToml, AppToolResultSourceFormat,
    AppToolResultSourceRequirementToml, AppToolsRequirementsToml, AppsRequirementsToml,
    AutoReviewRequirementsToml, ConfigRequirements, ConfigRequirementsToml,
    ConfigRequirementsWithSources, FeatureRequirementsToml, FilesystemConstraints,
    FilesystemDenyReadPattern, FilesystemRequirementsToml, MarketplaceAllowedSourceKind,
    MarketplaceAllowedSourceToml, MarketplaceRequirementsToml, ModelsRequirementsToml,
    NetworkConstraints, NetworkDomainPermissionToml, NetworkDomainPermissionsToml,
    NetworkHeaderInjectionToml, NetworkRequirementsToml, NetworkUnixSocketPermissionToml,
    NetworkUnixSocketPermissionsToml, NewThreadModelDefaultsToml, PermissionsRequirementsToml,
    RemoteSandboxConfigToml, ResidencyRequirement, SandboxModeRequirement,
    WebSearchModeRequirement, WindowsRequirementsToml, WindowsSandboxImplementationToml,
    sandbox_mode_requirement_for_permission_profile,
};
pub use in_app_browser_requirements::InAppBrowserRequirementsToml;
pub use ra_execpolicy::RequirementsExecPolicy;
pub use requirements_exec_policy::{
    RequirementsExecPolicyDecisionToml, RequirementsExecPolicyParseError,
    RequirementsExecPolicyPatternTokenToml, RequirementsExecPolicyPrefixRuleToml,
    RequirementsExecPolicyToml,
};

mod cloud_config_bundle;
mod cloud_config_layers;
pub mod diagnostics;
mod filesystem_constraints;
mod host_name;
mod model_provider_requirements;
mod path_resolution;
mod requirements_layers;
mod state;
pub mod strict_config;
pub use cloud_config_bundle::{
    CloudConfigBundle, CloudConfigBundleBinding, CloudConfigBundleBindingGuard,
    CloudConfigBundleBindingStatus, CloudConfigBundleLayers, CloudConfigBundleLoadError,
    CloudConfigBundleLoadErrorCode, CloudConfigBundleLoader, CloudConfigBundlePolicy,
    CloudConfigBundlePolicyChanges, CloudConfigBundlePolicyRevision, CloudConfigBundleSnapshot,
    CloudConfigTomlBundle, CloudRequirementsFragment, CloudRequirementsTomlBundle,
};
pub use cloud_config_layers::{
    CloudConfigFragment, CloudConfigFragmentSource, CloudConfigLayerError,
    cloud_config_layers_from_fragments,
};
pub use diagnostics::{ConfigError, ConfigLoadError, TextPosition, TextRange};
pub use host_name::{host_name, os_host_name};
pub use mcp_policy::PluginMcpRequirements as PluginRequirementsToml;
pub use requirements_layers::{
    RequirementsCompositionError, RequirementsLayerEntry, compose_requirements,
    compose_requirements_for_hostname,
};
pub use state::{ConfigLayerStack, ConfigLoadOptions, LoaderOverrides};
pub use toml::Value as TomlValue;

mod credential_sources;
mod git_trust;
pub mod loader;
mod project_root_markers;
mod thread_config;
pub use project_root_markers::{default_project_root_markers, project_root_markers_from_config};
pub use thread_config::{
    NoopThreadConfigLoader, SessionThreadConfig, StaticThreadConfigLoader, ThreadConfigContext,
    ThreadConfigLoadError, ThreadConfigLoadErrorCode, ThreadConfigLoader, ThreadConfigLoaderFuture,
    ThreadConfigSource, UserThreadConfig,
};

pub use git_trust::{resolve_root_git_project_for_trust, resolve_root_git_project_uri_for_trust};
pub use shell_values::{
    EnvironmentVariablePattern, ShellEnvironmentPolicy, ShellEnvironmentPolicyFilter,
    ShellEnvironmentPolicyInherit,
};

#[cfg(feature = "test-api")]
#[doc(hidden)]
pub use requirements_layers::test_api as requirements_test_api;

pub mod shell_environment;
pub use shell_environment_policy::{
    ShellEnvironmentPolicyToml, validate_shell_environment_policy_filter_config,
};

#[cfg(feature = "test-api")]
#[doc(hidden)]
pub use path_context::test_api as path_context_test_api;

#[cfg(feature = "test-api")]
#[doc(hidden)]
pub use config_requirements::test_api as requirements_value_test_api;

mod home;
pub use home::find_ra_home;
#[cfg(feature = "test-api")]
#[doc(hidden)]
pub use home::test_find_ra_home_from_env;

#[cfg(feature = "test-api")]
#[doc(hidden)]
pub use cloud_config_layers::test_cloud_config_layers_from_fragments_strict;

mod managed_features;
pub use managed_features::{
    ManagedFeatures, validate_explicit_feature_settings_in_config_toml,
    validate_feature_requirements_in_config_toml,
};

#[cfg(feature = "test-api")]
#[doc(hidden)]
pub use mcp_ema::test_api as mcp_ema_test_api;

pub mod config_edit;
mod config_validation;
pub use config_edit::{ConfigEdit, ConfigEditsBuilder};
pub use config_validation::{
    deserialize_config_toml_with_base, validate_feature_requirements_for_config_toml,
};

mod permission_path;
pub mod permission_profile_catalog;
pub mod permission_profiles;
pub mod permission_selection;
