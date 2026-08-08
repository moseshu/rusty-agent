//! Shared MCP client layer.

#[cfg(feature = "http")]
pub mod http;
#[cfg(feature = "sse")]
pub mod sse;
#[cfg(feature = "stdio")]
pub mod stdio;
