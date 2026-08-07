//! # `ra-context`
//!
//! 上下文预算、压缩、淘汰、归档。
//!
//! **稳定性分级**：`Evolving`。压缩触发与淘汰策略会随实测调整，阈值不是契约。

pub mod archive;
pub mod budget;
pub mod compaction;
pub mod eviction;
pub mod preflight;
pub mod window;
