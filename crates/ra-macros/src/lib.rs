//! # `ra-macros`
//!
//! Procedural macros: `#[derive(ToolInput)]`. (An attribute-macro form, `#[tool]`, is planned;
//! its module is still a stub.)
//!
//! **Boundary**: it generates only the boilerplate and schema that the `ra-core` contracts need,
//! at compile time. It contains no runtime registration, IO, or provider logic.
//!
//! A proc-macro crate can export nothing but macros, so every internal module stays private.
//!
//! **Stability**: the macro's call shape is `Stable`, **its expansion is `Internal`** — do not
//! depend on the exact form of the generated code, which is an implementation detail.

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
