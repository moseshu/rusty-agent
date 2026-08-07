//! # `ra-mcp`
//!
//! MCP client（stdio/sse/http）与进程内工具服务器。
//!
//! **稳定性分级**：`Evolving`。MCP 侧的传输与过滤配置随上游协议演进。

pub mod approval;
pub mod client;
pub mod filter;
pub mod in_process;
