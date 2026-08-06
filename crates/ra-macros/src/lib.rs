//! # `ra-macros`
//!
//! 过程宏：`#[derive(ToolInput)]` 与 `#[tool]`。
//!
//! proc-macro crate 只能导出宏本身，因此内部模块一律私有。

mod strict;
mod tool_attr;
mod tool_input;
