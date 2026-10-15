//! Tool namespaces on the protocols that cannot carry them.
//!
//! A namespace is an `OpenAI` Responses feature. The reference's other backends refuse a function
//! tool that uses it (`ensure_function_tool_supports_responses_only_features`) rather than flatten
//! it, since a flattened name could collide with another tool's or route a call to the wrong one;
//! these adapters do the same.

use ra_core::{
    error::{Error, Result},
    model::ModelToolDefinition,
};

/// Refuses the first of `tools` advertised in a namespace, on `backend`: the reference's
/// `ensure_function_tool_supports_responses_only_features`.
pub(crate) fn ensure_unnamespaced(tools: &[ModelToolDefinition], backend: &str) -> Result<()> {
    for tool in tools {
        let mut features = Vec::new();
        if tool.namespace().is_some() {
            features.push("tool namespace");
        }
        if tool.allowed_callers().is_some() {
            features.push("allowed_callers");
        }
        if tool.output_json_schema().is_some() {
            features.push("output_json_schema");
        }
        if !features.is_empty() {
            return Err(Error::caller(format!(
                "The following function-tool features are only supported with OpenAI Responses \
             models: {}. Tool `{}` cannot be used with {backend}.",
                features.join(", "),
                tool.qualified_name()
            )));
        }
    }
    Ok(())
}
