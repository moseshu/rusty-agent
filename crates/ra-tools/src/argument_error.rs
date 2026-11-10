//! Argument errors the framework reports before a call reaches its tool.

use ra_core::{error::Error, tool::ToolArgumentDecodeError};

/// The decode error carried by `error` when the provider's argument text was not JSON, so the
/// framework answered the call without handing it to the tool.
///
/// Only [`ToolArgumentDecodeError::InvalidJson`] qualifies: every other decode error reaches a
/// tool through its own `decode_input` and is wrapped in its own failure type. A tool with custom
/// failure handling falls back to this when the error carries none of its own failure types, and
/// renders it as it renders its own argument errors.
pub(crate) fn unparsed_arguments_error(error: &Error) -> Option<ToolArgumentDecodeError> {
    match std::error::Error::source(error)?.downcast_ref::<ToolArgumentDecodeError>()? {
        decode @ ToolArgumentDecodeError::InvalidJson { .. } => Some(decode.clone()),
        _ => None,
    }
}

/// The parser diagnostic of [`unparsed_arguments_error`], as the skills tools quote it.
pub(crate) fn unparsed_arguments_diagnostic(error: &Error) -> Option<String> {
    match unparsed_arguments_error(error)? {
        ToolArgumentDecodeError::InvalidJson { message } => Some(message),
        _ => None,
    }
}

/// The reply Codex's handlers give a call whose arguments are not JSON, from their shared
/// `parse_arguments`: `failed to parse function arguments: {parser diagnostic}`.
pub(crate) fn unparsed_arguments_reply(error: &Error) -> Option<String> {
    unparsed_arguments_diagnostic(error)
        .map(|diagnostic| format!("failed to parse function arguments: {diagnostic}"))
}
