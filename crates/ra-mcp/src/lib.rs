//! # `ra-mcp`
//!
//! MCP client（stdio/sse/http）与进程内工具服务器。
//!
//! **边界**：负责 MCP 协议、连接与工具过滤，不运行 agent loop，不把 MCP 类型泄漏
//! 进 `ra-core`，也不替产品决定默认信任或审批策略。
//!
//! **稳定性分级**：`Evolving`。MCP 侧的传输与过滤配置随上游协议演进。

pub mod approval;
pub mod client;
pub mod filter;
pub mod in_process;
