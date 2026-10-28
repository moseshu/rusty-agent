//! # `ra-mcp`
//!
//! MCP clients (stdio/streamable HTTP) and the in-process tool server.
//!
//! **Boundary**: it owns the MCP protocol, connections, and tool filtering. It does not run the
//! agent loop, does not leak MCP types into `ra-core`, and does not decide a product's default
//! trust or approval policy.
//!
//! **Stability**: `Evolving`. MCP transports and filter configuration follow the upstream protocol.

#![doc = include_str!("../README.md")]

pub mod approval;
pub mod catalog;
pub mod client;
pub mod filter;
pub mod in_process;
pub mod manager;
pub mod metadata;
pub mod resolver;
#[cfg(feature = "http")]
mod rustls_provider;
pub mod server;
pub mod tool;

pub use manager::{McpServerManager, McpServerManagerOptions};
pub use server::McpServer;

// Protocol types stay in this adapter crate; hosts can implement an in-process MCP handler.
pub use rmcp;

pub mod plugin_config;

mod registration_catalog;
pub use registration_catalog::{
    McpCatalogBuilder, McpEnvironmentAuthority, McpPluginAttribution, McpServerConflict,
    McpServerConflictAction, McpServerRegistration, McpServerSource, ResolvedMcpCatalog,
    ResolvedMcpServer,
};

/// Credential authority supplied by the owner of a server registration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum McpCredentialPolicy {
    /// The controller may resolve credentials from its own environment.
    HostFallbackAllowed,
    /// Only the executor owning the selected source may resolve credentials.
    ExecutorOnly,
}

/// Session-selected MCP compatibility policy, retained with registration snapshots.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum McpProtocolMode {
    /// Use standard SDK MCP initialization and OAuth negotiation.
    #[default]
    Legacy,
    /// Explicitly request the upstream discovery and request lifecycle extension.
    V20260728,
}

#[cfg(feature = "test-api")]
#[doc(hidden)]
pub use registration_catalog::test_api as registration_test_api;
