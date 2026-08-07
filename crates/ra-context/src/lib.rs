//! # `ra-context`
//!
//! 上下文预算、压缩、淘汰、归档。
//!
//! **边界**：只提供协议中立的上下文变换，不发模型请求、不持久化会话，也不决定
//! 某个产品应保留什么业务内容；内容策略由产品通过公开输入提供。
//!
//! **稳定性分级**：`Evolving`。压缩触发与淘汰策略会随实测调整，阈值不是契约。

pub mod archive;
pub mod budget;
pub mod compaction;
pub mod eviction;
pub mod preflight;
pub mod window;
