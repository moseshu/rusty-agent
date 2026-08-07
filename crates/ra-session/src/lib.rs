//! # `ra-session`
//!
//! 双通道 rollout 事件日志、SessionStore、resume、fork、checkpoint。
//!
//! **边界**：只保存和重放已经定义的会话事件，不运行模型或工具，不解释产品语义，
//! 也不把具体本地存储结构暴露成 `ra-core` 契约。
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
