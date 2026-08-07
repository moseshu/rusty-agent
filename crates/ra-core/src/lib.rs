//! # `ra-core`
//!
//! 内核类型与契约：零实现，只有类型、trait、常量。框架宪法。
//!
//! **边界**：本 crate 只定义跨层共享的协议中立类型，不做 I/O、不选择 provider、
//! 不装配 loop，也不包含任何产品词汇。实现型服务必须依赖本 crate，而不能反向把
//! 服务实现引入这里。
//!
//! **稳定性分级**：`Stable`——它是框架宪法，下游直接 match 这里的枚举、实现这里的
//! trait。两处例外：[`state`] 的 `RunState` 字段与 [`trace::field`] 的字段名是
//! `Evolving`（可加不可删），[`step`] 的 turn 结算中间态是 `Internal`（一旦泄漏成
//! 公共 API，R1/R3 就再也重构不动）。

pub mod budget;
pub mod cancel;
pub mod capability;
pub mod compat;
pub mod config;
pub mod error;
pub mod guard;
pub mod hook;
pub mod item;
pub mod model;
pub mod permission;
pub mod prompt;
pub mod session;
pub mod state;
pub mod step;
pub mod tool;
pub mod trace;
pub mod usage;
