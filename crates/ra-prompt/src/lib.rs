//! # `ra-prompt`
//!
//! 提示词装配机制。本 crate 不含任何提示词内容。
//!
//! **稳定性分级**：`Evolving`。段名与装配顺序可加不可删，改语义要进 CHANGELOG——
//! 它们决定缓存前缀，动一下就影响命中率与成本。

pub mod assembler;
pub mod cache_plan;
pub mod dump;
pub mod reminder;
pub mod role;
pub mod section;
pub mod stability;
