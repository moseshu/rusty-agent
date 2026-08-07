//! # `ra-patch`
//!
//! V4A 补丁格式的解析与应用。
//!
//! **边界**：只处理补丁语法、匹配、应用和 diff 渲染，不负责文件权限、审批、沙箱
//! 或编码 agent 的编辑纪律。模糊匹配器是实现细节，不进入公开 API。
//!
//! **稳定性分级**：`Evolving`。V4A 格式本身由上游定义，这里的解析器 API 可加不可删。

pub mod apply;
pub(crate) mod fuzz;
pub mod parse;
pub mod render;
