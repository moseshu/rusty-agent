//! `OpenAI` HTTP and transport errors mapped into the provider-neutral taxonomy.

use ra_core::error::{Error, ProviderErrorKind};
use serde_json::Value;

pub(crate) fn transport_error(error: reqwest::Error) -> Error {
    let kind = if error.is_timeout() {
        ProviderErrorKind::Timeout
    } else {
        ProviderErrorKind::Network
    };
    Error::provider(kind, "OpenAI request transport failed").with_source(error)
}

pub(crate) fn decode_error(error: reqwest::Error) -> Error {
    Error::provider(
        ProviderErrorKind::Behavior,
        "OpenAI returned a non-JSON response",
    )
    .with_source(error)
}

/// Maps a failed HTTP response into the neutral taxonomy.
///
/// `request_id` is carried into the message because the failing call is exactly where it is worth
/// quoting to `OpenAI` support. R1-9 turns these into a structured `NormalizedProviderError`.
pub(crate) fn response_error(
    status: reqwest::StatusCode,
    payload: &Value,
    request_id: Option<&str>,
) -> Error {
    let error = payload.get("error").unwrap_or(payload);
    let code = error.get("code").and_then(Value::as_str);
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("OpenAI request failed");
    let request_id = request_id.map_or_else(String::new, |id| format!(" (request_id {id})"));
    let kind = if matches!(
        code,
        Some("context_length_exceeded" | "context_window_exceeded")
    ) {
        ProviderErrorKind::ContextOverflow
    } else {
        match status.as_u16() {
            401 | 403 => ProviderErrorKind::Auth,
            408 => ProviderErrorKind::Timeout,
            429 => ProviderErrorKind::RateLimit,
            500..=599 => ProviderErrorKind::ServerError,
            _ => ProviderErrorKind::BadRequest,
        }
    };
    Error::provider(kind, format!("OpenAI HTTP {status}{request_id}: {message}"))
}

pub(crate) fn behavior_error(message: impl Into<String>) -> Error {
    Error::provider(ProviderErrorKind::Behavior, message)
}
