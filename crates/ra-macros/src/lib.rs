//! # `ra-macros`
//!
//! 过程宏：`#[derive(ToolInput)]` 与 `#[tool]`。
//!
//! proc-macro crate 只能导出宏本身，因此内部模块一律私有。
//!
//! **稳定性分级**：宏的调用形式是 `Stable`，**展开产物是 `Internal`**——不要依赖
//! 生成代码的具体形状，那是实现细节。

mod strict;
mod tool_attr;
mod tool_input;
