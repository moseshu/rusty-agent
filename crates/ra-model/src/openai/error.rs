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
        error_code(payload),
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
    if let Some(code) = error_code(payload) {
        normalized = normalized.with_error_code(code);
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

fn error_code(payload: &Value) -> Option<&str> {
    let error = payload.get("error").unwrap_or(payload);
    error
        .get("code")
        .and_then(Value::as_str)
        .or_else(|| error.get("type").and_then(Value::as_str))
}

pub(crate) fn behavior_error(message: impl Into<String>) -> Error {
    Error::provider(ProviderErrorKind::Behavior, message)
}
