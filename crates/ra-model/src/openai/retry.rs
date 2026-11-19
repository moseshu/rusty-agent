//! Retry advice under the `OpenAI` adapter contract.
//!
//! The provider-neutral advice in [`crate::retry`] keeps any replay verdict the adapter stamped on
//! the error. The SDK's `get_openai_retry_advice` adds one more: an endpoint that answered a
//! request continuing provider-held state with a retryable status rejected it rather than accepting
//! it, and an endpoint that sent `x-should-retry: true` vouched for repeating it, so both are safe
//! to replay. That reading belongs to the `OpenAI` endpoints' contract, which is why it applies to
//! the `OpenAI` models only and not to every provider sharing the neutral advice.

use ra_core::model::{ModelRetryAdviceRequest, NormalizedProviderError, ReplaySafety, RetryAdvice};

/// The provider-neutral advice, with the SDK's replay verdict for a failure whose safety was
/// otherwise unknown.
///
/// The SDK decides in this order, and the order is what keeps the verdict honest:
///
/// 1. A verdict already stamped on the error stands; `x-should-retry: true` does not lift an
///    explicit [`ReplaySafety::Unsafe`].
/// 2. `x-should-retry: true` makes the replay safe; `false` gives no verdict, whatever the status.
/// 3. A failure with no HTTP status (a network failure or a timeout below the response) gives no
///    verdict. The SDK checks its connection and timeout error types here; this adapter reports a
///    408 response as a timeout too, so it tells them apart by whether a status arrived.
/// 4. A 408, 409, 429 or 5xx response makes the replay safe for a request continuing provider-held
///    state.
pub(crate) fn retry_advice(request: &ModelRetryAdviceRequest<'_>) -> Option<RetryAdvice> {
    let advice = crate::retry::retry_advice(request)?;
    if advice.replay_safety() != ReplaySafety::Unknown {
        return Some(advice);
    }
    let Some(normalized) = NormalizedProviderError::from_error(request.error()) else {
        return Some(advice);
    };
    let safe = match normalized.should_retry() {
        Some(should_retry) => should_retry,
        None => normalized.status_code().is_some_and(|status| {
            request.continuation().is_server_managed()
                && (matches!(status, 408 | 409 | 429) || status >= 500)
        }),
    };
    Some(if safe {
        advice.with_replay_safety(ReplaySafety::Safe)
    } else {
        advice
    })
}
