//! MCP client 公共层。

#[cfg(feature = "http")]
pub mod http;
#[cfg(feature = "sse")]
pub mod sse;
#[cfg(feature = "stdio")]
pub mod stdio;
