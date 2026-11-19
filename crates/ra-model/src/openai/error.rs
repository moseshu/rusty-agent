// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! `OpenAI` HTTP and transport errors mapped into the provider-neutral taxonomy.
//!
//! Every failure here is built as a [`NormalizedProviderError`] and converted, so the status, the
//! vendor code, the request identifier and any requested delay reach a retry policy as fields. The
//! message keeps quoting the request identifier as well: that string is what a human pastes into a
//! support ticket, and it is worth having in the one line that reaches a log.

use ra_core::{
    error::{Error, ProviderErrorKind},
    model::NormalizedProviderError,
};
use serde_json::Value;

use crate::retry::RetryHints;

#[derive(Debug, thiserror::Error)]
#[error("OpenAI error type {0}")]
struct ErrorType(String, Option<String>);

/// Returns the provider's error `type`, independently of its error `code`.
///
/// Both fields can be present. Product policies use the type to recognize usage and quota
/// failures while the normalized error retains the original code.
#[must_use]
pub fn error_type(error: &Error) -> Option<&str> {
    let mut source = std::error::Error::source(error);
    while let Some(current) = source {
        if let Some(error_type) = current.downcast_ref::<ErrorType>() {
            return Some(&error_type.0);
        }
        source = current.source();
    }
    None
}

/// Returns the provider's explicit error `code`, without falling back to its `type`.
/// The normalized framework field retains its existing code-or-type fallback.
#[must_use]
pub fn error_code(error: &Error) -> Option<&str> {
    let mut source = std::error::Error::source(error);
    while let Some(current) = source {
        if let Some(metadata) = current.downcast_ref::<ErrorType>() {
            return metadata.1.as_deref();
        }
        source = current.source();
    }
    NormalizedProviderError::from_error(error).and_then(NormalizedProviderError::error_code)
}

/// Whether the request failed before it reached the endpoint because no connection could be
/// established, as Codex's HTTP transport reports `TransportError::Connection` for a client error
/// that `is_connect()`. This includes connection timeouts for which `is_connect()` is true;
/// other timeouts, failures after the connection was made, and WebSocket failures are excluded.
#[must_use]
pub fn is_connection_failure(error: &Error) -> bool {
    let mut source = std::error::Error::source(error);
    while let Some(current) = source {
        if let Some(http) = current.downcast_ref::<ra_http::HttpError>() {
            return http.is_connect();
        }
        if let Some(request) = current.downcast_ref::<reqwest::Error>() {
            return request.is_connect();
        }
        source = current.source();
    }
    false
}

/// Header carrying the endpoint's diagnostic identifier for one request.
const REQUEST_ID_HEADER: &str = "x-request-id";

/// What a response states about itself before its body is consumed.
///
/// Read once, up front: the body is a stream that can only be taken by value, and after taking it
/// the headers that say how to handle a failure are no longer reachable.
pub(crate) struct ResponseFacts {
    status: reqwest::StatusCode,
    request_id: Option<String>,
    hints: RetryHints,
}

impl ResponseFacts {
    pub(crate) fn read(response: &reqwest::Response) -> Self {
        Self::from_headers(response.status(), response.headers())
    }

    pub(crate) fn from_headers(
        status: reqwest::StatusCode,
        headers: &reqwest::header::HeaderMap,
    ) -> Self {
        let lookup = |name: &str| headers.get(name).and_then(|value| value.to_str().ok());
        Self {
            status,
            request_id: lookup(REQUEST_ID_HEADER).map(str::to_owned),
            hints: RetryHints::from_headers(lookup),
        }
    }

    pub(crate) fn failure_as(
        &self,
        payload: &Value,
        kind: ProviderErrorKind,
    ) -> NormalizedProviderError {
        response_failure_as(
            Some(self.status),
            self.request_id.as_deref(),
            &self.hints,
            payload,
            kind,
        )
    }

    pub(crate) const fn status(&self) -> reqwest::StatusCode {
        self.status
    }

    /// The diagnostic identifier, for the successful path that carries it into the response.
    pub(crate) fn into_request_id(self) -> Option<String> {
        self.request_id
    }
}

pub(crate) fn transport_error(error: reqwest::Error) -> Error {
    let kind = if error.is_timeout() {
        ProviderErrorKind::Timeout
    } else {
        ProviderErrorKind::Network
    };
    NormalizedProviderError::new(kind, "OpenAI request transport failed")
        .with_source(error)
        .into_error()
}

pub(crate) fn decode_error(error: reqwest::Error) -> Error {
    NormalizedProviderError::new(
        ProviderErrorKind::Behavior,
        "OpenAI returned a non-JSON response",
    )
    .with_source(error)
    .into_error()
}

pub(crate) fn http_transport_error(error: ra_http::HttpError) -> Error {
    let kind = if matches!(
        error,
        ra_http::HttpError::Policy(_) | ra_http::HttpError::Build(_)
    ) || error.is_builder()
    {
        ProviderErrorKind::BadRequest
    } else if error.is_timeout() {
        ProviderErrorKind::Timeout
    } else {
        ProviderErrorKind::Network
    };
    NormalizedProviderError::new(kind, "OpenAI request transport failed")
        .with_source(error.without_url())
        .into_error()
}

pub(crate) fn http_decode_error(error: ra_http::HttpError) -> Error {
    if let ra_http::HttpError::Request(error) = error {
        return decode_error(error.without_url());
    }
    http_transport_error(error)
}

/// Maps a failed HTTP response into the neutral taxonomy.
///
/// The record is returned rather than the error so a caller that also holds the failure which
/// stopped it from reading the body can attach that as the source. Building the error first and
/// adding the source afterwards would overwrite these facts with it.
///
/// # What the status mapping deliberately leaves alone
///
/// The vendor's code is recorded, not mapped. `insufficient_quota` and `rate_limit_exceeded` both
/// arrive as 429 and mean opposite things about whether waiting helps, but resolving that is a
/// policy question about who is willing to pay, not a classification this layer can settle.
pub(crate) fn response_failure(facts: &ResponseFacts, payload: &Value) -> NormalizedProviderError {
    let kind = if matches!(
        payload_error_code(payload),
        Some("context_length_exceeded" | "context_window_exceeded")
    ) {
        ProviderErrorKind::ContextOverflow
    } else {
        match facts.status.as_u16() {
            401 | 403 => ProviderErrorKind::Auth,
            408 => ProviderErrorKind::Timeout,
            // A well-formed request that lost a race with a concurrent change. The reference
            // client retries it, and so does this classification.
            409 => ProviderErrorKind::Conflict,
            429 => ProviderErrorKind::RateLimit,
            500..=599 => ProviderErrorKind::ServerError,
            _ => ProviderErrorKind::BadRequest,
        }
    };

    facts.failure_as(payload, kind)
}

pub(crate) fn response_failure_as(
    status: Option<reqwest::StatusCode>,
    request_id: Option<&str>,
    hints: &RetryHints,
    payload: &Value,
    kind: ProviderErrorKind,
) -> NormalizedProviderError {
    let message = payload
        .get("error")
        .unwrap_or(payload)
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("OpenAI request failed");
    let quoted_request_id = request_id.map_or_else(String::new, |id| format!(" (request_id {id})"));
    let mut normalized = if let Some(status) = status {
        NormalizedProviderError::new(
            kind,
            format!("OpenAI HTTP {status}{quoted_request_id}: {message}"),
        )
        .with_status_code(status.as_u16())
    } else {
        NormalizedProviderError::new(
            kind,
            format!("OpenAI Responses WebSocket{quoted_request_id}: {message}"),
        )
    };
    if let Some(code) = payload_error_code(payload) {
        normalized = normalized.with_error_code(code);
    }
    if let Some(error_type) = payload
        .get("error")
        .unwrap_or(payload)
        .get("type")
        .and_then(Value::as_str)
    {
        normalized = normalized.with_source(ErrorType(
            error_type.to_owned(),
            payload
                .get("error")
                .unwrap_or(payload)
                .get("code")
                .and_then(Value::as_str)
                .map(str::to_owned),
        ));
    }
    if let Some(request_id) = request_id {
        normalized = normalized.with_request_id(request_id);
    }
    if let Some(retry_after) = hints.retry_after() {
        normalized = normalized.with_retry_after(retry_after);
    }
    if let Some(should_retry) = hints.should_retry() {
        normalized = normalized.with_should_retry(should_retry);
    }
    normalized
}

fn payload_error_code(payload: &Value) -> Option<&str> {
    let error = payload.get("error").unwrap_or(payload);
    error
        .get("code")
        .and_then(Value::as_str)
        .or_else(|| error.get("type").and_then(Value::as_str))
}

pub(crate) fn behavior_error(message: impl Into<String>) -> Error {
    Error::provider(ProviderErrorKind::Behavior, message)
}

// Provider event identity is kept in the adapter, so product retry policies can distinguish an
// HTTP refusal from a terminal stream frame without changing the framework error taxonomy.
#[derive(Debug, thiserror::Error)]
#[error("OpenAI stream failure {event}")]
struct ResponseStreamFailure {
    event: String,
    incomplete_reason: Option<String>,
    /// The `code` of the frame's nested `error` object, which Codex's stream parsers read.
    error_code: Option<String>,
}

fn stream_failure_metadata(error: &Error) -> Option<&ResponseStreamFailure> {
    let mut source = std::error::Error::source(error);
    while let Some(current) = source {
        if let Some(metadata) = current.downcast_ref::<ResponseStreamFailure>() {
            return Some(metadata);
        }
        source = current.source();
    }
    None
}

/// The Responses event that terminated a failed stream, if the adapter received one.
#[must_use]
pub fn response_failure_event(error: &Error) -> Option<&str> {
    stream_failure_metadata(error).map(|metadata| metadata.event.as_str())
}

/// The provider's incomplete-response reason, independently of its error code.
#[must_use]
pub fn response_incomplete_reason(error: &Error) -> Option<&str> {
    stream_failure_metadata(error).and_then(|metadata| metadata.incomplete_reason.as_deref())
}

/// The `code` of a failed stream frame's nested `error` object: `response.error.code` for a
/// terminal response event, `error.code` for an `error` frame.
///
/// Unlike the normalized error code, this does not fall back to a code outside an `error`
/// object, as Codex's stream parsers do not.
#[must_use]
pub fn response_stream_error_code(error: &Error) -> Option<&str> {
    stream_failure_metadata(error).and_then(|metadata| metadata.error_code.as_deref())
}

static RATE_LIMIT_DELAY: std::sync::LazyLock<Option<regex_lite::Regex>> =
    std::sync::LazyLock::new(|| {
        regex_lite::Regex::new(r"(?i)try again in\s*(\d+(?:\.\d+)?)\s*(s|ms|seconds?)").ok()
    });

pub(crate) fn stream_failure(
    event: &str,
    frame: &Value,
    kind: ProviderErrorKind,
    message: String,
) -> Error {
    let response = frame.get("response").unwrap_or(frame);
    let error = response.get("error").unwrap_or(response);
    let mut normalized = NormalizedProviderError::new(kind, message);
    if let Some(code) = error.get("code").and_then(Value::as_str) {
        normalized = normalized.with_error_code(code);
    }
    // Codex reads a stream failure's wait advice only from a `response.failed` frame's nested
    // `response.error`; every other failure frame, and a failed response without that object,
    // ends without advice and backs off.
    let retry_after = (event == "response.failed")
        .then(|| frame.get("response")?.get("error"))
        .flatten()
        .filter(|error| error.is_object())
        .and_then(failed_response_retry_after);
    if let Some(delay) = retry_after {
        normalized = normalized.with_retry_after(delay);
    }
    normalized
        .with_source(ResponseStreamFailure {
            event: event.to_owned(),
            incomplete_reason: response
                .get("incomplete_details")
                .and_then(|details| details.get("reason"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            error_code: response
                .get("error")
                .and_then(|error| error.get("code"))
                .and_then(Value::as_str)
                .map(str::to_owned),
        })
        .into_error()
}

/// The wait advice of a failed response's `error` object: its `headers`, or for a rate limit the
/// delay its message names, as Codex's `parse_failed_response` reads them.
fn failed_response_retry_after(error: &Value) -> Option<std::time::Duration> {
    let hints = RetryHints::from_headers(|name| {
        error
            .get("headers")?
            .as_object()?
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))?
            .1
            .as_str()
    });
    hints.retry_after().or_else(|| {
        if !matches!(
            error.get("code").and_then(Value::as_str),
            Some("rate_limit_exceeded" | "slow_down")
        ) {
            return None;
        }
        // Codex's Responses rate-limit parser also accepts the delay embedded in its message.
        let captures = RATE_LIMIT_DELAY
            .as_ref()?
            .captures(error.get("message")?.as_str()?)?;
        let value = captures.get(1)?.as_str().parse::<f64>().ok()?;
        if captures.get(2)?.as_str().eq_ignore_ascii_case("ms") {
            std::time::Duration::try_from_secs_f64(value.trunc() / 1000.0).ok()
        } else {
            std::time::Duration::try_from_secs_f64(value).ok()
        }
    })
}
