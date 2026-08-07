//! # `ra-exec`
//!
//! 进程执行、PTY、后台 job、沙箱隔离。
//!
//! **边界**：只执行已经成型的命令与安全策略，不决定工具是否应该被调用，也不包含
//! 产品默认权限；审批策略属于 runtime / 产品装配，命令内容属于调用方。
//!
//! **稳定性分级**：`SandboxBackend` 是 extension trait，`Stable`；沙箱策略与
//! manifest 字段是 `Evolving`（平台后端会增加）。

pub mod command;
pub mod job;
pub mod output;
pub mod pty;
pub mod sandbox;
pub mod session;
