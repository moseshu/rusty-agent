//! # `ra-protocol`
//!
//! 双向控制协议、transport、app-server。
//!
//! **边界**：负责宿主与 agent 进程之间的线协议和生命周期，不实现 runner、会话
//! 存储或 UI；持久化委托 `ra-session`，执行委托上层装配。
//!
//! **稳定性分级**：`Evolving`。帧是与宿主之间的线协议，**只能加字段不能删**，
//! 且未知字段必须原样保留——两端版本不会同步升级。

pub mod control;
pub mod frame;
pub mod lifecycle;
pub mod server;
pub mod subscribe;
pub mod transport;
