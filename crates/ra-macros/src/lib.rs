//! # `ra-macros`
//!
//! 过程宏：`#[derive(ToolInput)]`。（`#[tool]` 属性宏见 R2-4，尚未落地。）
//!
//! **边界**：只在编译期生成 `ra-core` 契约所需的样板与 schema，不包含运行时注册、
//! I/O 或 provider 逻辑。
//!
//! proc-macro crate 只能导出宏本身，因此内部模块一律私有。
//!
//! **稳定性分级**：宏的调用形式是 `Stable`，**展开产物是 `Internal`**——不要依赖
//! 生成代码的具体形状，那是实现细节。

mod strict;
mod tool_attr;
mod tool_input;

use proc_macro::TokenStream;

/// Derives the `ra_core::tool::ToolInput` metadata contract for a serde/schemars struct.
#[proc_macro_derive(ToolInput, attributes(tool_input))]
pub fn derive_tool_input(input: TokenStream) -> TokenStream {
    tool_input::expand(input.into())
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}
