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
//! longer reaches the advice. A WebSocket transport attaches evidence of where its request failed,
//! as the SDK's WebSocket model marks its errors, and the Responses model interprets that evidence
//! before the generic advice.

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

/// Marks a disconnect while sending the request or before receiving the first response event.
pub(crate) fn mark_post_send_websocket_disconnect(error: Error) -> Error {
    mark_websocket_failure(error, WebSocketFailure::PostSendDisconnect)
}

/// Marks a send, or a read before the first response event, that timed out.
pub(crate) fn mark_post_send_websocket_timeout(error: Error) -> Error {
    mark_websocket_failure(error, WebSocketFailure::PostSendTimeout)
}

/// Marks an `error` frame received before the first response event; `transient` when it reports
/// an overloaded server, or a server error with no code.
pub(crate) fn mark_pre_response_error_frame(error: Error, transient: bool) -> Error {
    mark_websocket_failure(error, WebSocketFailure::ErrorFrame { transient })
}

/// Marks a WebSocket handshake that failed without a response status or a disconnect, such as a
/// malformed response, a refused connection or a TLS failure.
pub(crate) fn mark_failed_websocket_handshake(error: Error) -> Error {
    mark_websocket_failure(error, WebSocketFailure::FailedHandshake)
}

/// The error for a request that the provider's `close` ended or refused.
pub(crate) fn closed_websocket_error() -> Error {
    Error::caller("Responses WebSocket provider has been closed").with_source(
        WebSocketFailureEvidence {
            failure: WebSocketFailure::Closed,
            source: None,
        },
    )
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
        _ => normalized,
    };
    normalized
        .map_source(|source| {
            Some(Box::new(WebSocketFailureEvidence { failure, source }) as BoxError)
        })
        .into_error()
}

/// Where a Responses WebSocket request failed, as the SDK's WebSocket model records it on the
/// exception it raises.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WebSocketFailure {
    /// A request lock wait or connect timeout, or a disconnect during the handshake: the SDK's
    /// `safe` replay marker and its never-sent disconnect.
    Unsent,
    /// A disconnect after the SDK began sending the request and before the first response event:
    /// its `unsafe` replay marker without a started response.
    PostSendDisconnect,
    /// The SDK's `send` and `receive` timeout phases.
    PostSendTimeout,
    /// An `error` frame before the first event, the SDK's `ResponsesWebSocketError`. Only a
    /// transient one, `server_is_overloaded` or a code-less `server_error`, gets advice: the SDK's
    /// error type exposes no status or headers a Codex frame may carry.
    ErrorFrame { transient: bool },
    /// A handshake failure the SDK does not treat as a disconnect, which gets no advice.
    FailedHandshake,
    /// The SDK's close invalidation.
    Closed,
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
            WebSocketFailure::PostSendTimeout => {
                "Responses WebSocket timed out after sending the request"
            }
            WebSocketFailure::ErrorFrame { .. } => {
                "Responses WebSocket reported an error before responding"
            }
            WebSocketFailure::FailedHandshake => "Responses WebSocket handshake failed",
            WebSocketFailure::Closed => "Responses WebSocket provider was closed",
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
/// 1. A request the provider's `close` ended or refused is not retried.
/// 2. A response that had started is unsafe to replay. A disconnect or timeout after sending the
///    request, before the first response event, is too when the request continues provider-held
///    state; otherwise it can be retried without asserting replay safety.
/// 3. An overloaded server, or a server error with no code, reported in an `error` frame before
///    the first response event can be retried without a verdict, whatever the continuation. Any
///    other such frame gets no advice, whatever status or headers it carries.
/// 4. Another explicit [`ReplaySafety::Unsafe`] verdict refuses the replay.
/// 5. A transient WebSocket failure known to predate sending the request can be safely retried.
/// 6. A handshake that failed without a response status or a disconnect gets no advice.
///
/// The generic advice follows. A rejected HTTP upgrade uses it through its status and headers.
///
/// 7. `x-should-retry: true` suggests a replay as safe; `false` refuses a retry without a verdict.
/// 8. A failure with no HTTP status, a network failure or a timeout below the response, suggests a
///    retry. The SDK checks its connection and timeout error types here; this adapter reports a 408
///    response as a timeout too, so it tells them apart by whether a status arrived.
/// 9. A 408, 409, 429 or 5xx response suggests a retry, and calls the replay safe for a request
///    continuing provider-held state, whose refusal shows the endpoint did not accept it. The
///    error's classification does not matter here, so a context overflow reported with one of
///    these statuses is still suggested.
/// 10. Otherwise a server delay is passed on without a suggestion, and with none there is no
///     advice.
///
/// A failure raised before the request left this process gets no advice, as the provider-neutral
/// advice gives none: it is not provider evidence.
pub(crate) fn retry_advice(request: &ModelRetryAdviceRequest<'_>) -> Option<RetryAdvice> {
    let error = request.error();
    let websocket_failure = websocket_failure(error);
    if let (Some(WebSocketFailure::Closed), Error::Caller { message, .. }) =
        (websocket_failure, error)
    {
        // Use the runner's streaming veto to represent the SDK's close-invalidation abort.
        // This says nothing about whether the request was accepted. It relies on Responses
        // WebSocket calls being evaluated as streaming; a non-streaming policy can approve
        // unsafe replay explicitly.
        return Some(
            RetryAdvice::new()
                .with_suggested(false)
                .with_replay_safety(ReplaySafety::Unsafe)
                .with_reason(message.clone()),
        );
    }
    let Error::Provider { kind, message, .. } = error else {
        return None;
    };
    let normalized = NormalizedProviderError::from_error(error);
    let stateful = request.continuation().is_server_managed();
    let advice = || RetryAdvice::new().with_reason(message.clone());
    let started = normalized.is_some_and(NormalizedProviderError::response_started);
    let refusal = || {
        advice()
            .with_suggested(false)
            .with_replay_safety(ReplaySafety::Unsafe)
            .with_response_started(started)
    };

    match websocket_failure {
        _ if started => return Some(refusal()),
        Some(WebSocketFailure::PostSendDisconnect | WebSocketFailure::PostSendTimeout) => {
            return Some(if stateful {
                refusal()
            } else {
                advice().with_suggested(true)
            });
        }
        Some(WebSocketFailure::ErrorFrame { transient }) => {
            return transient.then(|| advice().with_suggested(true));
        }
        _ => {}
    }
    // The transport stamps the failures above by its own replay boundary, which is not the SDK's
    // evidence. It stamps the ones below as safe, so an unsafe verdict on them was restated later.
    if replay_safety_of(error) == ReplaySafety::Unsafe {
        return Some(refusal());
    }
    match websocket_failure {
        Some(WebSocketFailure::Unsent) => {
            return Some(
                advice()
                    .with_suggested(true)
                    .with_replay_safety(ReplaySafety::Safe),
            );
        }
        Some(WebSocketFailure::FailedHandshake) => return None,
        _ => {}
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
