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

use std::{fmt, sync::Arc, time::Duration};

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
///
/// Equality and `Debug` cover the decision's public meaning only. The markers policy combinators
/// read are left out, as the SDK declares its private fields with `compare=False` and `repr=False`.
#[non_exhaustive]
#[derive(Clone)]
pub struct RetryDecision {
    retry: bool,
    delay: Option<Duration>,
    reason: Option<String>,
    unsafe_replay_approved: bool,
    /// The SDK's `_hard_veto` and `_delegable_replay_veto`, which only policy combinators read.
    veto: Veto,
    /// The SDK's `_approves_replay`: the provider called the replay safe. Only
    /// [`ProviderSuggestedRetryPolicy`] sets it, and combinators carry it into their decision.
    provider_replay_approved: bool,
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
            veto: Veto::None,
            provider_replay_approved: false,
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
            veto: Veto::None,
            provider_replay_approved: false,
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

    /// The SDK's `_with_hard_veto`.
    const fn with_hard_veto(mut self) -> Self {
        self.veto = Veto::Hard;
        self
    }

    /// The SDK's `_with_delegable_replay_veto`.
    const fn with_delegable_replay_veto(mut self) -> Self {
        self.veto = Veto::DelegableReplay;
        self
    }

    /// The SDK's `_with_replay_safe_approval`.
    const fn with_provider_replay_approval(mut self) -> Self {
        self.provider_replay_approved = true;
        self
    }

    /// The SDK's `_merge_positive_retry_decisions`: a later positive decision's delay and reason
    /// replace earlier ones, and approvals accumulate.
    fn merge_positive(mut self, incoming: Self) -> Self {
        let mut merged = Self::retry();
        merged.delay = incoming.delay.or(self.delay);
        merged.reason = incoming.reason.or_else(|| self.reason.take());
        merged.unsafe_replay_approved =
            self.unsafe_replay_approved || incoming.unsafe_replay_approved;
        merged.provider_replay_approved =
            self.provider_replay_approved || incoming.provider_replay_approved;
        merged
    }

    /// The SDK's `_resolve_delegable_replay_veto`: the veto stands unless the approving decision
    /// retries with an unsafe-replay approval, which then replaces it.
    fn resolve_delegable_veto(self, approving: Self) -> Self {
        if !approving.retry || !approving.unsafe_replay_approved {
            return self;
        }
        let mut resolved = Self::retry().with_unsafe_replay_approval();
        resolved.delay = approving.delay;
        // The SDK's `approving.reason or veto.reason`: an empty reason falls back too.
        resolved.reason = approving
            .reason
            .filter(|reason| !reason.is_empty())
            .or(self.reason);
        resolved.provider_replay_approved = approving.provider_replay_approved;
        resolved
    }
}

impl PartialEq for RetryDecision {
    fn eq(&self, other: &Self) -> bool {
        self.retry == other.retry
            && self.delay == other.delay
            && self.reason == other.reason
            && self.unsafe_replay_approved == other.unsafe_replay_approved
    }
}

impl Eq for RetryDecision {}

#[allow(
    clippy::missing_fields_in_debug,
    reason = "The SDK hides the combinator markers with repr=False."
)]
impl fmt::Debug for RetryDecision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RetryDecision")
            .field("retry", &self.retry)
            .field("delay", &self.delay)
            .field("reason", &self.reason)
            .field("unsafe_replay_approved", &self.unsafe_replay_approved)
            .finish()
    }
}

/// How a policy combinator treats a decision not to retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Veto {
    /// An ordinary decision, weighed with the others.
    None,
    /// Ends the combined evaluation with this decision.
    Hard,
    /// A hard veto over an unsafe replay, which another combined policy's unsafe-replay approval
    /// can resolve.
    DelegableReplay,
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
/// As the SDK's `provider_suggested`, a provider veto is a hard veto for [`AllRetryPolicy`] and
/// [`AnyRetryPolicy`]: one over a replay the adapter marked unsafe can be resolved by another
/// combined policy's unsafe-replay approval, any other cannot. A positive suggestion the adapter
/// called safe to replay carries that approval into the combined decision.
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
                let mut decision = RetryDecision::no_retry();
                if let Some(reason) = advice.reason() {
                    decision = decision.with_reason(reason);
                }
                return if advice.replay_safety() == ReplaySafety::Unsafe {
                    decision.with_delegable_replay_veto()
                } else {
                    decision.with_hard_veto()
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
        if advice.replay_safety() == ReplaySafety::Safe {
            decision = decision.with_provider_replay_approval();
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

/// A policy that retries only when every combined policy does, as the SDK's `retry_policies.all`.
///
/// Policies are asked in order. A hard veto ends the evaluation with that veto, and so does the
/// first decision not to retry. A provider veto over an unsafe replay is set aside instead, and
/// stands unless the combined decision approves unsafe replay. Later delays and reasons replace
/// earlier ones, and approvals accumulate. With no policies it never retries.
pub struct AllRetryPolicy {
    policies: Vec<Arc<dyn ModelRetryPolicy>>,
}

impl AllRetryPolicy {
    /// Combines `policies`, asked in the given order.
    #[must_use]
    pub fn new(policies: impl IntoIterator<Item = Arc<dyn ModelRetryPolicy>>) -> Self {
        Self {
            policies: policies.into_iter().collect(),
        }
    }
}

impl fmt::Debug for AllRetryPolicy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AllRetryPolicy")
            .field("policies", &self.policies.len())
            .finish()
    }
}

#[async_trait]
impl ModelRetryPolicy for AllRetryPolicy {
    async fn evaluate(&self, context: &RetryPolicyContext<'_>) -> RetryDecision {
        if self.policies.is_empty() {
            return RetryDecision::no_retry();
        }
        let mut merged = RetryDecision::retry();
        let mut delegable_veto = None;
        for policy in &self.policies {
            let decision = policy.evaluate(context).await;
            match decision.veto {
                Veto::DelegableReplay => {
                    delegable_veto.get_or_insert(decision);
                    continue;
                }
                Veto::Hard => return decision,
                Veto::None => {}
            }
            if !decision.retry {
                return decision;
            }
            if decision.delay.is_some() {
                merged.delay = decision.delay;
            }
            if decision.reason.is_some() {
                merged.reason = decision.reason;
            }
            merged.unsafe_replay_approved |= decision.unsafe_replay_approved;
            merged.provider_replay_approved |= decision.provider_replay_approved;
        }
        match delegable_veto {
            Some(veto) => veto.resolve_delegable_veto(merged),
            None => merged,
        }
    }
}

/// A policy that retries when any combined policy does, as the SDK's `retry_policies.any`.
///
/// Every policy is asked, in order, unless one returns a hard veto, which ends the evaluation with
/// that veto. A provider veto over an unsafe replay is set aside instead, and stands unless the
/// positive decisions approve unsafe replay. Positive decisions merge, later delays and reasons
/// replacing earlier ones; without one, the last decision not to retry is returned. With no
/// policies it never retries.
pub struct AnyRetryPolicy {
    policies: Vec<Arc<dyn ModelRetryPolicy>>,
}

impl AnyRetryPolicy {
    /// Combines `policies`, asked in the given order.
    #[must_use]
    pub fn new(policies: impl IntoIterator<Item = Arc<dyn ModelRetryPolicy>>) -> Self {
        Self {
            policies: policies.into_iter().collect(),
        }
    }
}

impl fmt::Debug for AnyRetryPolicy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AnyRetryPolicy")
            .field("policies", &self.policies.len())
            .finish()
    }
}

#[async_trait]
impl ModelRetryPolicy for AnyRetryPolicy {
    async fn evaluate(&self, context: &RetryPolicyContext<'_>) -> RetryDecision {
        let mut first_positive: Option<RetryDecision> = None;
        let mut last_negative = None;
        let mut delegable_veto = None;
        for policy in &self.policies {
            let decision = policy.evaluate(context).await;
            match decision.veto {
                Veto::DelegableReplay => {
                    delegable_veto.get_or_insert(decision);
                    continue;
                }
                Veto::Hard => return decision,
                Veto::None => {}
            }
            if decision.retry {
                first_positive = Some(match first_positive {
                    Some(earlier) => earlier.merge_positive(decision),
                    None => decision,
                });
                continue;
            }
            last_negative = Some(decision);
        }
        match (delegable_veto, first_positive) {
            (Some(veto), Some(positive)) => veto.resolve_delegable_veto(positive),
            (Some(veto), None) => veto,
            (None, Some(positive)) => positive,
            (None, None) => last_negative.unwrap_or_else(RetryDecision::no_retry),
        }
    }
}

/// A policy that retries the failures whose HTTP status is one of the given codes, as the SDK's
/// `retry_policies.http_status`.
#[derive(Debug, Clone, Default)]
pub struct HttpStatusRetryPolicy {
    statuses: Vec<u16>,
}

impl HttpStatusRetryPolicy {
    /// Retries failures answered with any of `statuses`.
    #[must_use]
    pub fn new(statuses: impl IntoIterator<Item = u16>) -> Self {
        Self {
            statuses: statuses.into_iter().collect(),
        }
    }
}

#[async_trait]
impl ModelRetryPolicy for HttpStatusRetryPolicy {
    async fn evaluate(&self, context: &RetryPolicyContext<'_>) -> RetryDecision {
        match context
            .normalized()
            .and_then(NormalizedProviderError::status_code)
        {
            Some(status) if self.statuses.contains(&status) => RetryDecision::retry(),
            _ => RetryDecision::no_retry(),
        }
    }
}
