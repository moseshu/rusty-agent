//! # `ra-session`
//!
//! 双通道 rollout 事件日志、SessionStore、resume、fork、checkpoint。
//!
//! **稳定性分级**：`Evolving`。rollout 事件 payload 与存储 schema 可加不可删，
//! 且必须能读旧版本——resume 依赖它。

pub mod chain;
pub mod checkpoint;
pub mod file_history;
pub mod lite;
pub mod mutate;
pub mod resume;
pub mod rollout;
pub mod store;
