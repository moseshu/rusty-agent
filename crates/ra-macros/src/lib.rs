//! # `ra-macros`
//!
//! Procedural macros: `#[derive(ToolInput)]` and `#[tool]` function tools.
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

/// Creates a tool constructor at `function_name::tool()` while preserving the function.
///
/// Accepts sync and async free functions with owned, deserializable JSON arguments. The first
/// argument may be `ToolContext<'_>`, `&ToolContext<'_>`, or `&RunContext`; it is injected rather
/// than advertised. Sync bodies run on Tokio's blocking pool.
///
/// Options: `name`, `description`, `strict`, `use_docstring_info`, `is_enabled`,
/// `needs_approval`, `failure_error_function`, `timeout` (seconds or a `Duration`; async functions
/// only, as upstream), `timeout_behavior`, `tool_input_guardrails`, `tool_output_guardrails`
/// (registered guardrail IDs), `defer_loading`, `allowed_callers`, and `options`, an adjustment
/// `|options| ...` applied to the function-tool defaults before the explicit keywords.
/// `timeout_error_function` accepts `(&ToolContext, &ToolTimeoutError)` and returns a
/// `Result` containing a serializable value or native `ToolOutput`. `custom_data_extractor`
/// accepts `&mut FunctionToolCustomDataContext` and returns `Result<Option<Map<String, Value>>>`.
/// Both accept an async callback path, `sync(callback)`, or `None`.
/// `output_type = MyType` generates a strict object schema and Serde validation;
/// `output_json_schema = expression` supplies a strict object schema directly. They are mutually
/// exclusive. Structured returns infer an output type when `allowed_callers` includes
/// `"programmatic"`; strings, unit, and `serde_json::Value` retain untyped output behavior.
/// Boolean policies accept a literal, an async callback path, or `sync(callback)`. Failure
/// callbacks accept `(&ToolContext, &Error)` and return `Result<Option<ToolOutput>>`;
/// `failure_error_function = None` selects error propagation.
/// Parameter `#[tool(description = "...", default = expression)]` supplies field metadata.
/// Generated `options()` returns the declared policies, and `tool_with_options(options)` replaces
/// them for one instance; start from `options()` to keep the defaults.
///
/// The function must be a module item: the generated module reaches it through `super::`, so a
/// declaration inside a function body does not resolve. Argument types and default expressions are
/// resolved from that module too, so a `super::` path there names one level too high; use
/// `crate::` or an import instead.
#[proc_macro_attribute]
pub fn tool(attributes: TokenStream, input: TokenStream) -> TokenStream {
    tool_attr::expand(attributes.into(), input.into())
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}
