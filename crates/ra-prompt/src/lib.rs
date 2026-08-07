//! # `ra-prompt`
//!
//! 提示词装配机制。本 crate 不含任何提示词内容。
//!
//! **边界**：只负责分段、排序、稳定前缀与缓存计划；身份、工具偏好和业务纪律等
//! 文本属于产品 crate，本 crate 不读取配置文件，也不调用 provider。
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
