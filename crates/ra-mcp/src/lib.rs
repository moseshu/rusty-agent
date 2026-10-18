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
pub mod resolver;
#[cfg(feature = "http")]
mod rustls_provider;
pub mod server;
pub mod tool;

pub use server::McpServer;

// Protocol types stay in this adapter crate; hosts can implement an in-process MCP handler.
pub use rmcp;
