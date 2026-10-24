//! Product plugin packages, Host manifests, explicit local installation, and command migration.
//!
//! These entries follow Codex's package formats. They do not read product configuration,
//! create MCP connections, register agents, or execute hook declarations.
//! The Store installs only explicitly supplied local directories; it does not enable plugins.
//! Host resources use the existing absolute-path type; generic manifest resource mapping remains
//! available without claiming executor filesystem or URI authority support.

pub mod command_migration;
pub mod discovery;
pub mod hook_config;
pub mod identity;
pub mod installed;
pub mod manifest;
pub mod model;
pub mod store;

pub use identity::{PluginId, PluginIdError};
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
