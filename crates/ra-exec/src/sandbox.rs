//! trait `SandboxBackend` + `SecurityPolicy`。
//!
//! 后端按 feature × 平台双重门控：feature 表达"我要这个后端"，`target_os` 表达
//! "这个平台上它才存在"。默认构建在 macOS 得到 [`seatbelt`]、在 Linux 得到
//! [`bwrap`]，[`unix_local`] 是两边都在的基线。
//!
//! **不可静默降级**：某个平台上没有任何真沙箱后端可用时，构造阶段必须报错并说清
//! 缺的是什么，不能悄悄退回 [`unix_local`] 让调用方以为自己被隔离着。

#[cfg(all(feature = "bwrap", target_os = "linux"))]
pub mod bwrap;
#[cfg(feature = "docker")]
pub mod docker;
pub mod manifest;
pub mod network;
#[cfg(all(feature = "seatbelt", target_os = "macos"))]
pub mod seatbelt;
pub mod unix_local;
