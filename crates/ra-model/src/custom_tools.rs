//! Custom tools on protocols that have no custom form.
//!
//! A custom tool takes one raw string rather than JSON arguments (see
//! [`ModelToolKind`](ra_core::model::ModelToolKind)). `OpenAI` Responses carries it natively. The
//! reference refuses one on Chat Completions — its converter raises for any tool that is not a
//! function — and Anthropic Messages has nothing to carry it in, so both adapters refuse it by
//! default.
//!
//! A caller can opt either adapter into advertising a custom tool as a function taking one string
//! argument, `input`, and lifting a call to it back into a custom call carrying that string. That is
//! this framework's extension, not the reference's behaviour: the shape is the one codex used for
//! `apply_patch` before it removed its function form, and it loses what the custom form exists
//! for — the grammar is not sent, so nothing but the tool's own parser checks the input.

use std::collections::BTreeSet;

use ra_core::{
    error::{Error, Result},
    item::ToolCall,
    model::{ModelRequest, ModelToolDefinition},
};
use serde_json::{Value, json};

/// The one argument a custom tool advertised as a function takes.
pub(crate) const FUNCTION_INPUT: &str = "input";

/// The parameters of a custom tool advertised as a function.
pub(crate) fn function_parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            FUNCTION_INPUT: {
                "type": "string",
                "description": "The tool's entire raw input, written exactly as the tool's \
                                description specifies.",
            }
        },
        "required": [FUNCTION_INPUT],
        "additionalProperties": false,
    })
}

/// A custom call replayed as a call to the function it was advertised as.
///
/// # Errors
///
/// Returns a caller error when the call's arguments are not its raw input string.
pub(crate) fn function_arguments(call: &ToolCall) -> Result<Value> {
    let input = call.arguments().as_str().ok_or_else(|| {
        Error::caller(format!(
            "custom tool call `{}` has arguments that are not its raw input string",
            call.call_id().as_str()
        ))
    })?;
    Ok(json!({ FUNCTION_INPUT: input }))
}

/// The custom tools in `tools`.
pub(crate) fn names(tools: &[ModelToolDefinition]) -> BTreeSet<String> {
    tools
        .iter()
        .filter(|tool| tool.kind().is_custom())
        .map(|tool| tool.name().to_owned())
        .collect()
}

/// The custom tools a request advertises as functions, or none when the adapter does not.
pub(crate) fn advertised_as_functions(enabled: bool, request: &ModelRequest) -> BTreeSet<String> {
    if enabled {
        names(request.tools())
    } else {
        BTreeSet::new()
    }
}

/// Refuses a custom tool, or a custom call in history, on a protocol that cannot carry it.
pub(crate) fn unsupported(protocol: &str, name: &str) -> Error {
    Error::caller(format!(
        "custom tool `{name}` is not supported with {protocol}; custom tools require OpenAI \
         Responses, or opting this adapter into advertising custom tools as functions"
    ))
}
