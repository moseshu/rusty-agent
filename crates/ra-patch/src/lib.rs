//! # `ra-patch`
//!
//! V4A 补丁格式的解析与应用。
//!
//! **稳定性分级**：`Evolving`。V4A 格式本身由上游定义，这里的解析器 API 可加不可删。

pub mod apply;
pub mod fuzz;
pub mod parse;
pub mod render;
