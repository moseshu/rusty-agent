//! Product plugin packages, Host manifests, source materialization, and local installation.
//!
//! These entries follow Codex's package formats. They do not read product configuration,
//! create MCP connections, register agents, or execute hook declarations.
//! The Store installs only explicitly supplied local directories; it does not enable plugins.
//! Explicit materialization runs Git/npm and owns temporary source files through the returned value.
//! Plugin activation and marketplace configuration values belong to [`crate::config`].
//! Host resources use native absolute paths. URI parsing and inert environment-bound descriptors
//! preserve resource ownership without selecting or connecting an executor filesystem.

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
