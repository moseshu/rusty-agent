//! The runtime policy that decides whether a failed model call is worth another attempt.
//!
//! Provider adapters report facts through [`RetryAdvice`]; this module keeps
//! the application decision separate from those facts. As in the `OpenAI` Agents SDK, the runtime
//! asks the policy about every failure except a cancellation, whatever its
//! [`Recoverability`](crate::error::Recoverability), so a policy filters the failures it accepts:
//! check [`Error::is_retryable`] to retry transient failures only. A policy cannot bypass the
//! runtime's attempt cap, cancellation, a stream event that was already published, or a streamed
//! failure the adapter marked unsafe to replay.
//!
//! Replay protection follows the SDK too. An ordinary [`RetryDecision::retry`] never replays a
//! failure the adapter marked unsafe, nor a request continuing provider-held state whose safety
//! the adapter did not establish. [`RetryDecision::with_unsafe_replay_approval`] lifts only the
//! first, and only for a non-streaming call; replay safety the adapter left unknown cannot be
//! approved by a policy.

use std::time::Duration;

use async_trait::async_trait;

use super::{NormalizedProviderError, ReplaySafety, RetryAdvice, replay_safety_of};
use crate::{error::Error, model::request::ConversationContinuation};

/// The continuation of a context built without one: the request carried its own history.
static CLIENT_HISTORY: ConversationContinuation = ConversationContinuation::None;

/// Context supplied to a retry policy after one failed model attempt.
#[non_exhaustive]
#[derive(Debug)]
pub struct RetryPolicyContext<'a> {
    error: &'a Error,
    attempt: u32,
    max_retries: u32,
    streaming: bool,
    provider_advice: Option<&'a RetryAdvice>,
    continuation: &'a ConversationContinuation,
}

impl<'a> RetryPolicyContext<'a> {
    /// Creates context for the attempt that just failed.
    #[must_use]
    pub const fn new(
        error: &'a Error,
        attempt: u32,
        max_retries: u32,
        streaming: bool,
        provider_advice: Option<&'a RetryAdvice>,
    ) -> Self {
        Self {
            error,
            attempt,
            max_retries,
            streaming,
            provider_advice,
            continuation: &CLIENT_HISTORY,
        }
    }

    /// Records how the failed request continued the conversation. Without it the request is taken
    /// to have carried its own history.
    #[must_use]
    pub const fn with_continuation(mut self, continuation: &'a ConversationContinuation) -> Self {
        self.continuation = continuation;
        self
    }

    /// The error returned by the failed attempt.
    #[must_use]
    pub const fn error(&self) -> &Error {
        self.error
    }

    /// The failed attempt's number within the retry budget, as in the `OpenAI` Agents SDK: `1` is
    /// the first failure, and a retry is allowed while it does not exceed [`Self::max_retries`]. A
    /// retry that does not consume the budget does not advance it, so this is not a count of
    /// requests sent.
    #[must_use]
    pub const fn attempt(&self) -> u32 {
        self.attempt
    }

    /// Number of retry attempts allowed after the initial call.
    #[must_use]
    pub const fn max_retries(&self) -> u32 {
        self.max_retries
    }

    /// Whether the failed call used the streaming entry point.
    #[must_use]
    pub const fn is_streaming(&self) -> bool {
        self.streaming
    }

    /// Provider evidence, if the adapter supplied any.
    #[must_use]
    pub const fn provider_advice(&self) -> Option<&RetryAdvice> {
        self.provider_advice
    }

    /// Normalized provider facts carried by [`Self::error`], if any.
    #[must_use]
    pub fn normalized(&self) -> Option<&NormalizedProviderError> {
        NormalizedProviderError::from_error(self.error)
    }

    /// The replay verdict the runtime applies to this failure: the adapter's advice, or else the
    /// verdict stamped on the error.
    ///
    /// The context only lends the advice, so a policy cannot change the verdict it is judged
    /// against.
    #[must_use]
    pub fn replay_safety(&self) -> ReplaySafety {
        self.provider_advice
            .map_or_else(|| replay_safety_of(self.error), RetryAdvice::replay_safety)
    }

    /// Whether the provider had begun emitting the response when the failure occurred.
    #[must_use]
    pub fn response_started(&self) -> bool {
        self.provider_advice
            .is_some_and(RetryAdvice::response_started)
    }

    /// How the failed request continued the conversation.
    #[must_use]
    pub const fn continuation(&self) -> &ConversationContinuation {
        self.continuation
    }

    /// Whether the request continued provider-held state, through a previous response ID or a
    /// provider conversation.
    #[must_use]
    pub const fn is_stateful_request(&self) -> bool {
        self.continuation.is_server_managed()
    }
}

/// A retry policy's answer for one failed attempt.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryDecision {
    retry: bool,
    delay: Option<Duration>,
    reason: Option<String>,
    unsafe_replay_approved: bool,
}

impl RetryDecision {
    /// Declines another attempt.
    #[must_use]
    pub const fn no_retry() -> Self {
        Self {
            retry: false,
            delay: None,
            reason: None,
            unsafe_replay_approved: false,
        }
    }

    /// Permits another attempt, subject to the runtime safety checks.
    #[must_use]
    pub const fn retry() -> Self {
        Self {
            retry: true,
            delay: None,
            reason: None,
            unsafe_replay_approved: false,
        }
    }

    /// Supplies a delay that takes precedence over the configured backoff.
    #[must_use]
    pub const fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = Some(delay);
        self
    }

    /// Adds a stable diagnostic reason for the decision.
    #[must_use]
    pub fn with_reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }

    /// Approves replaying a request the adapter marked [`ReplaySafety::Unsafe`], as the SDK's
    /// `approve_unsafe_replay`.
    ///
    /// This is separate from [`Self::retry`]: an ordinary retry never bypasses replay protection.
    /// Set it only for workloads where repeating provider-side work that may already have happened
    /// is acceptable. It applies to a non-streaming call only, and also to one that continues
    /// provider-held state. It does not lift a stream event that already reached a subscriber, a
    /// streamed failure, or a stateful request whose replay safety the adapter left
    /// [`ReplaySafety::Unknown`].
    #[must_use]
    pub const fn with_unsafe_replay_approval(mut self) -> Self {
        self.unsafe_replay_approved = true;
        self
    }

    /// Whether another attempt was requested.
    #[must_use]
    pub const fn should_retry(&self) -> bool {
        self.retry
    }

    /// Optional policy-selected delay.
    #[must_use]
    pub const fn delay(&self) -> Option<Duration> {
        self.delay
    }

    /// Stable diagnostic reason, if one was supplied.
    #[must_use]
    pub fn reason(&self) -> Option<&str> {
        self.reason.as_deref()
    }

    /// Whether the policy approved replaying a request the adapter marked unsafe.
    #[must_use]
    pub const fn approves_unsafe_replay(&self) -> bool {
        self.unsafe_replay_approved
    }
}

/// Runtime-only decision procedure for model-call retries.
#[async_trait]
pub trait ModelRetryPolicy: Send + Sync {
    /// Decides whether the failed attempt merits another call.
    ///
    /// It is called for non-transient failures too, such as a rejected request or an invalid
    /// credential; a policy that accepts everything retries those.
    async fn evaluate(&self, context: &RetryPolicyContext<'_>) -> RetryDecision;
}

/// A policy that never retries.
#[derive(Debug, Default)]
pub struct NeverRetryPolicy;

#[async_trait]
impl ModelRetryPolicy for NeverRetryPolicy {
    async fn evaluate(&self, _context: &RetryPolicyContext<'_>) -> RetryDecision {
        RetryDecision::no_retry()
    }
}

/// A policy that follows an adapter's positive retry recommendation.
///
/// The SDK's version also marks its decision when the adapter called the replay safe. That marker
/// only matters to policy combinators, which are not ported; the runtime reads the adapter's
/// verdict directly.
#[derive(Debug, Default)]
pub struct ProviderSuggestedRetryPolicy;

#[async_trait]
impl ModelRetryPolicy for ProviderSuggestedRetryPolicy {
    async fn evaluate(&self, context: &RetryPolicyContext<'_>) -> RetryDecision {
        let Some(advice) = context.provider_advice() else {
            return RetryDecision::no_retry();
        };
        match advice.suggested() {
            None => return RetryDecision::no_retry(),
            // A provider veto keeps its reason, as the SDK's does.
            Some(false) => {
                let decision = RetryDecision::no_retry();
                return match advice.reason() {
                    Some(reason) => decision.with_reason(reason),
                    None => decision,
                };
            }
            Some(true) => {}
        }
        let mut decision = RetryDecision::retry();
        if let Some(delay) = advice.retry_after() {
            decision = decision.with_delay(delay);
        }
        if let Some(reason) = advice.reason() {
            decision = decision.with_reason(reason);
        }
        decision
    }
}

/// A policy that retries network and timeout provider failures.
#[derive(Debug, Default)]
pub struct NetworkErrorRetryPolicy;

#[async_trait]
impl ModelRetryPolicy for NetworkErrorRetryPolicy {
    async fn evaluate(&self, context: &RetryPolicyContext<'_>) -> RetryDecision {
        match context.normalized() {
            Some(normalized) if normalized.is_network_error() || normalized.is_timeout() => {
                RetryDecision::retry()
            }
            _ => RetryDecision::no_retry(),
        }
    }
}

/// A policy that retries only when a provider asked the client to wait.
#[derive(Debug, Default)]
pub struct RetryAfterPolicy;

#[async_trait]
impl ModelRetryPolicy for RetryAfterPolicy {
    async fn evaluate(&self, context: &RetryPolicyContext<'_>) -> RetryDecision {
        let delay = context
            .provider_advice()
            .and_then(RetryAdvice::retry_after)
            .or_else(|| {
                context
                    .normalized()
                    .and_then(NormalizedProviderError::retry_after)
            });
        match delay {
            Some(delay) => RetryDecision::retry().with_delay(delay),
            None => RetryDecision::no_retry(),
        }
    }
}
