//! Product plugin metadata, host manifest loading, and read-only installed version selection.
//!
//! These entries follow Codex's package formats. They do not read product configuration,
//! install packages, create MCP connections, register agents, or execute hook declarations.
//! Host resources use the existing absolute-path type; generic manifest resource mapping remains
//! available without claiming executor filesystem or URI authority support.

pub mod discovery;
pub mod hook_config;
pub mod identity;
pub mod installed;
pub mod manifest;
pub mod model;

pub use identity::{PluginId, PluginIdError};
/// The existing native absolute-path representation used by host package resources.
pub use ra_execpolicy::AbsolutePathBuf;
