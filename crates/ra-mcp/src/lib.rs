//! # `ra-mcp`
//!
//! MCP clients (stdio/sse/http) and the in-process tool server.
//!
//! **Boundary**: it owns the MCP protocol, connections, and tool filtering. It does not run the
//! agent loop, does not leak MCP types into `ra-core`, and does not decide a product's default
//! trust or approval policy.
//!
//! **Stability**: `Evolving`. MCP transports and filter configuration follow the upstream protocol.

pub mod approval;
pub mod client;
pub mod filter;
pub mod in_process;
