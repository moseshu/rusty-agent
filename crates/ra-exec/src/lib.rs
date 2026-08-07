//! # `ra-exec`
//!
//! 进程执行、PTY、后台 job、沙箱隔离。
//!
//! **稳定性分级**：`SandboxBackend` 是 extension trait，`Stable`；沙箱策略与
//! manifest 字段是 `Evolving`（平台后端会增加）。

pub mod command;
pub mod job;
pub mod output;
pub mod pty;
pub mod sandbox;
pub mod session;
