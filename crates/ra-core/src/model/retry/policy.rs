//! The runtime policy that decides whether a failed model call is worth another attempt.
//!
//! Provider adapters report facts through [`RetryAdvice`](super::RetryAdvice); this module keeps
//! the application decision separate from those facts. In particular, a retry policy can choose
//! a narrower class of transient failures than an adapter recognizes, but it cannot bypass the
//! runtime's attempt cap, cancellation, or a stream event that was already published.

use std::time::Duration;

use async_trait::async_trait;

use super::{NormalizedProviderError, RetryAdvice};
use crate::error::Error;

/// Context supplied to a retry policy after one failed model attempt.
#[non_exhaustive]
#[derive(Debug)]
pub struct RetryPolicyContext<'a> {
    error: &'a Error,
    attempt: u32,
    max_retries: u32,
    streaming: bool,
    provider_advice: Option<&'a RetryAdvice>,
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
        }
    }

    /// The error returned by the failed attempt.
    #[must_use]
    pub const fn error(&self) -> &Error {
        self.error
    }

    /// Zero-based number of the failed attempt.
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
}

/// A retry policy's answer for one failed attempt.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryDecision {
    retry: bool,
    delay: Option<Duration>,
    reason: Option<String>,
    replay_approved: bool,
}

impl RetryDecision {
    /// Declines another attempt.
    #[must_use]
    pub const fn no_retry() -> Self {
        Self {
            retry: false,
            delay: None,
            reason: None,
            replay_approved: false,
        }
    }

    /// Permits another attempt, subject to the runtime safety checks.
    #[must_use]
    pub const fn retry() -> Self {
        Self {
            retry: true,
            delay: None,
            reason: None,
            replay_approved: false,
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

    /// Explicitly approves replay where the adapter could not establish safety.
    ///
    /// This is intentionally explicit. It cannot override an unsafe verdict or a stream event
    /// that has already reached the run subscriber, but it lets a host that owns stronger
    /// idempotency evidence permit a server-managed request whose adapter correctly reported
    /// [`super::ReplaySafety::Unknown`].
    #[must_use]
    pub const fn with_replay_approval(mut self) -> Self {
        self.replay_approved = true;
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

    /// Whether the policy explicitly accepts an otherwise-unknown replay boundary.
    #[must_use]
    pub const fn replay_approved(&self) -> bool {
        self.replay_approved
    }
}

/// Runtime-only decision procedure for model-call retries.
#[async_trait]
pub trait ModelRetryPolicy: Send + Sync {
    /// Decides whether the failed attempt merits another call.
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
#[derive(Debug, Default)]
pub struct ProviderSuggestedRetryPolicy;

#[async_trait]
impl ModelRetryPolicy for ProviderSuggestedRetryPolicy {
    async fn evaluate(&self, context: &RetryPolicyContext<'_>) -> RetryDecision {
        let Some(advice) = context.provider_advice() else {
            return RetryDecision::no_retry();
        };
        if advice.suggested() != Some(true) {
            return RetryDecision::no_retry();
        }
        let mut decision = RetryDecision::retry();
        if let Some(delay) = advice.retry_after() {
            decision = decision.with_delay(delay);
        }
        if let Some(reason) = advice.reason() {
            decision = decision.with_reason(reason);
        }
        if matches!(advice.replay_safety(), super::ReplaySafety::Safe) {
            decision = decision.with_replay_approval();
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
