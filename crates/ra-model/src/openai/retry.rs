//! Retry advice under the `OpenAI` adapter contract.
//!
//! Ported from the `OpenAI` Agents SDK's `get_openai_retry_advice`
//! (`src/agents/models/_openai_retry.py`), which both the Chat Completions and the Responses models
//! answer with, and from the Responses model's `get_retry_advice`
//! (`src/agents/models/openai_responses.py`), which first honors a replay verdict its WebSocket
//! transport attached to the failure; at f53c62451c7f34c15f0c87fa745e05404025420f, see NOTICE for
//! the retained MIT license.
//!
//! The advice reads the endpoint's response, not the adapter's own stamps: a verdict an HTTP path
//! stamps on a failure whose request carries its own history is not evidence the SDK has, so it no
//! longer reaches the advice. A WebSocket transport can attach evidence of a pre-send failure or
//! a disconnect before its first response event, which the Responses model interprets separately.

use std::fmt;

use ra_core::{
    error::{BoxError, Error, ProviderErrorKind},
    model::{
        ModelRetryAdviceRequest, NormalizedProviderError, ReplaySafety, RetryAdvice,
        replay_safety_of,
    },
};

/// Marks a transient WebSocket failure known to have happened before the request was sent.
///
/// The mark is kept inside the normalized record's source chain, so a later restatement of the
/// verdict, such as the runtime's after output was consumed, keeps it and every other fact.
pub(crate) fn mark_unsent_websocket_failure(error: Error) -> Error {
    mark_websocket_failure(error, WebSocketFailure::Unsent)
}

/// Marks a disconnect after sending the request and before receiving the first response event.
pub(crate) fn mark_post_send_websocket_disconnect(error: Error) -> Error {
    mark_websocket_failure(error, WebSocketFailure::PostSendDisconnect)
}

fn mark_websocket_failure(error: Error, failure: WebSocketFailure) -> Error {
    let Error::Provider {
        kind,
        message,
        source,
        ..
    } = error
    else {
        return error;
    };
    let normalized = match source {
        Some(source) => match source.downcast::<NormalizedProviderError>() {
            Ok(normalized) => *normalized,
            Err(source) => NormalizedProviderError::new(kind, message).with_source(source),
        },
        None => NormalizedProviderError::new(kind, message),
    };
    let normalized = match failure {
        WebSocketFailure::Unsent => normalized.with_replay_safety(ReplaySafety::Safe),
        WebSocketFailure::PostSendDisconnect => normalized,
    };
    normalized
        .map_source(|source| {
            Some(Box::new(WebSocketFailureEvidence { failure, source }) as BoxError)
        })
        .into_error()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WebSocketFailure {
    Unsent,
    PostSendDisconnect,
}

#[derive(Debug)]
struct WebSocketFailureEvidence {
    failure: WebSocketFailure,
    source: Option<BoxError>,
}

impl fmt::Display for WebSocketFailureEvidence {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self.failure {
            WebSocketFailure::Unsent => "Responses WebSocket request was not sent",
            WebSocketFailure::PostSendDisconnect => {
                "Responses WebSocket disconnected after sending the request"
            }
        })
    }
}

impl std::error::Error for WebSocketFailureEvidence {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn std::error::Error + 'static))
    }
}

fn websocket_failure(error: &Error) -> Option<WebSocketFailure> {
    let mut source = std::error::Error::source(error);
    while let Some(current) = source {
        if let Some(evidence) = current.downcast_ref::<WebSocketFailureEvidence>() {
            return Some(evidence.failure);
        }
        source = current.source();
    }
    None
}

/// The SDK's advice for a failed `OpenAI` call, in its order.
///
/// 1. A WebSocket disconnect after sending can be retried when there is no provider-held
///    continuation and the response has not started, without asserting replay safety. Other
///    explicit [`ReplaySafety::Unsafe`] verdicts refuse the replay.
/// 2. A transient WebSocket failure known to predate sending the request can be safely retried.
///    A rejected HTTP upgrade still uses the status and response headers below.
/// 3. `x-should-retry: true` suggests a replay as safe; `false` refuses a retry without a verdict.
/// 4. A failure with no HTTP status, a network failure or a timeout below the response, suggests a
///    retry. The SDK checks its connection and timeout error types here; this adapter reports a 408
///    response as a timeout too, so it tells them apart by whether a status arrived.
/// 5. A 408, 409, 429 or 5xx response suggests a retry, and calls the replay safe for a request
///    continuing provider-held state, whose refusal shows the endpoint did not accept it. The
///    error's classification does not matter here, so a context overflow reported with one of
///    these statuses is still suggested.
/// 6. Otherwise a server delay is passed on without a suggestion, and with none there is no advice.
///
/// A failure raised before the request left this process gets no advice, as the provider-neutral
/// advice gives none: it is not provider evidence.
pub(crate) fn retry_advice(request: &ModelRetryAdviceRequest<'_>) -> Option<RetryAdvice> {
    let error = request.error();
    let Error::Provider { kind, message, .. } = error else {
        return None;
    };
    let normalized = NormalizedProviderError::from_error(error);
    let stateful = request.continuation().is_server_managed();
    let advice = || RetryAdvice::new().with_reason(message.clone());

    let started = normalized.is_some_and(NormalizedProviderError::response_started);
    let websocket_failure = websocket_failure(error);
    let post_send_disconnect = websocket_failure == Some(WebSocketFailure::PostSendDisconnect);
    if post_send_disconnect && !stateful && !started {
        return Some(advice().with_suggested(true));
    }
    if post_send_disconnect || replay_safety_of(error) == ReplaySafety::Unsafe {
        return Some(
            advice()
                .with_suggested(false)
                .with_replay_safety(ReplaySafety::Unsafe)
                .with_response_started(started),
        );
    }
    if websocket_failure == Some(WebSocketFailure::Unsent) {
        return Some(
            advice()
                .with_suggested(true)
                .with_replay_safety(ReplaySafety::Safe),
        );
    }

    let retry_after = normalized.and_then(NormalizedProviderError::retry_after);
    let with_delay = |advice: RetryAdvice| match retry_after {
        Some(delay) => advice.with_retry_after(delay),
        None => advice,
    };
    match normalized.and_then(NormalizedProviderError::should_retry) {
        Some(true) => {
            return Some(with_delay(
                advice()
                    .with_suggested(true)
                    .with_replay_safety(ReplaySafety::Safe),
            ));
        }
        Some(false) => return Some(with_delay(advice().with_suggested(false))),
        None => {}
    }

    let status = normalized.and_then(NormalizedProviderError::status_code);
    if status.is_none()
        && matches!(
            kind,
            ProviderErrorKind::Network | ProviderErrorKind::Timeout
        )
    {
        return Some(with_delay(advice().with_suggested(true)));
    }
    if status.is_some_and(|status| matches!(status, 408 | 409 | 429) || status >= 500) {
        let suggested = with_delay(advice().with_suggested(true));
        return Some(if stateful {
            suggested.with_replay_safety(ReplaySafety::Safe)
        } else {
            suggested
        });
    }
    retry_after.map(|delay| advice().with_retry_after(delay))
}
