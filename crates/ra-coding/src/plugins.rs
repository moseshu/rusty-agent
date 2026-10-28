//! Product plugin packages, Host manifests, source materialization, and local installation.
//!
//! These entries follow Codex's package formats. They do not read product configuration,
//! create MCP connections, register agents, or execute hook declarations.
//! The Store installs only explicitly supplied local directories; it does not enable plugins.
//! Explicit materialization runs Git/npm and owns temporary source files through the returned value.
//! Plugin activation and marketplace configuration values belong to [`crate::config`].
//! Host resources use native absolute paths. Executor providers resolve explicitly selected
//! roots through their owning environment and retain that filesystem for later resource and MCP
//! reads. MCP policy overlays and inline cleanup admission return declarations for the product's
//! registration and dispatch consumers. URI descriptors preserve the source environment identity.

mod bundled_hooks;
mod executor_hook_source;
mod executor_hooks;
mod executor_mcp;
mod executor_provider;
pub use executor_hook_source::ExecutorPluginHookSource;
pub use executor_hooks::executor_plugin_hook_sources;
pub use executor_mcp::{PluginMcpProvider, PluginMcpProviderError};
pub use executor_provider::{
    ExecutorPluginProvider, ExecutorPluginProviderError, PluginRootOwnership,
    ResolvedExecutorPlugin,
};
pub mod command_migration;
pub mod discovery;
mod git_policy;
pub mod hook_config;
pub mod identity;
pub mod installed;
pub mod manifest;
pub mod marketplace;
pub mod marketplace_add;
pub mod materialize;
pub mod model;
mod npm_source;
mod plugin_bundle_archive;
mod process;
mod provider;
pub mod store;

pub use bundled_hooks::is_allowlisted_bundled_cleanup_hook;
pub use identity::{PluginId, PluginIdError};
pub use provider::{
    PluginResourceLocator, ResolvedPlugin, ResolvedPluginError, ResolvedPluginLocation,
};
/// The existing native absolute-path representation used by host package resources.
pub use ra_execpolicy::AbsolutePathBuf;

// The source path type has an infallible join; an absolute base preserves that invariant here.
#[allow(clippy::expect_used)]
pub(crate) fn join_absolute_path(
    base: &AbsolutePathBuf,
    path: impl AsRef<std::path::Path>,
) -> AbsolutePathBuf {
    let expanded = crate::permissions::path::expand_home_directory(path.as_ref());
    AbsolutePathBuf::from_absolute_path(base.as_path().join(expanded))
        .expect("joining an absolute path preserves its root")
}

mod mcp_policy;
pub use mcp_policy::apply_configured_plugin_mcp_server_policies;

#[cfg(feature = "test-api")]
#[doc(hidden)]
pub use executor_provider::test_api as executor_provider_test_api;

#[cfg(feature = "test-api")]
#[doc(hidden)]
pub use executor_mcp::test_api as executor_mcp_test_api;

/// Activation candidates extracted from explicit configuration writes.
pub mod toggles;

mod http_client_selector;
pub mod installed_marketplaces;
mod marketplace_policy;
pub mod marketplace_remove;
pub mod marketplace_upgrade;
pub mod remote;
pub mod remote_legacy;
pub mod startup_sync;
pub(crate) use git_policy::{PluginGitMode, configure_trusted_git_repository};
pub use marketplace_policy::allowed_configured_marketplace_names;

/// Name of the curated marketplace snapshot synchronized at startup.
pub const OPENAI_CURATED_MARKETPLACE_NAME: &str = "openai-curated";
/// Name of the curated marketplace used by API-key product installs.
pub const OPENAI_API_CURATED_MARKETPLACE_NAME: &str = "openai-api-curated";
/// Name of the marketplace bundled with the product package.
pub const OPENAI_BUNDLED_MARKETPLACE_NAME: &str = "openai-bundled";
pub(crate) const OPENAI_BUNDLED_ALPHA_MARKETPLACE_NAME: &str = "openai-bundled-alpha";
pub(crate) const OPENAI_PRIMARY_RUNTIME_MARKETPLACE_NAME: &str = "openai-primary-runtime";
/// Whether `name` is one of the curated marketplace names.
#[must_use]
pub fn is_openai_curated_marketplace_name(name: &str) -> bool {
    name == OPENAI_CURATED_MARKETPLACE_NAME || name == OPENAI_API_CURATED_MARKETPLACE_NAME
}

mod skill_roots;
pub use crate::skills::SkillDiscoveryMode;
pub use discovery::{
    DISCOVERABLE_PLUGIN_MANIFEST_PATHS, migrated_command_skills_root, plugin_namespace_for_root_uri,
};
pub use skill_roots::{PluginIdentity, PluginSkillRoot};

mod contributions;
mod load_outcome;
pub use contributions::{
    AppConnectorId, AppDeclaration, PluginCapabilitySummary, PluginHookSource,
    app_connector_ids_from_declarations,
};
pub use load_outcome::{LoadedPlugin, PluginLoadOutcome, prompt_safe_plugin_description};

mod app_config;
pub mod loader;

mod manager;
#[cfg(feature = "test-api")]
pub use manager::test_api as manager_test_api;
mod skill_snapshots;
pub use manager::{
    ConfiguredMarketplace, ConfiguredMarketplaceListOutcome, ConfiguredMarketplacePlugin,
    EffectivePluginsChange, EffectivePluginsChangedCallback, PluginDetail,
    PluginDetailsUnavailableReason, PluginHookSummary, PluginInstallError, PluginInstallOutcome,
    PluginInstallRequest, PluginMarketplaceContext, PluginMarketplaceScope, PluginReadOutcome,
    PluginReadRequest, PluginUninstallError, PluginsConfigInput, PluginsManager,
};

mod runtime_config;
pub use runtime_config::PluginRuntimeConfig;

mod mentions;
mod render;
pub use mentions::{
    CollectedToolMentions, collect_explicit_app_ids, collect_explicit_plugin_ids,
    collect_explicit_plugin_mentions, collect_tool_mentions_from_messages,
};
pub use render::{render_explicit_plugin_instructions, render_plugins_section};

mod services;
pub use services::{
    ConsumerBinding, ConsumerReconcile, PluginConfigManager, PluginServices, PluginSnapshot,
};
mod skill_provider;
pub use skill_provider::PluginSkillProvider;

mod protocol;
