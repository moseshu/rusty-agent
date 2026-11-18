// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Turn-level retry of product sampling requests.
//!
//! Codex retries a failed sampling stream from its turn loop, up to the provider's
//! `stream_max_retries`, when the error yields a retry delay: the server's advised delay when one
//! was sent, or an exponential backoff otherwise. Some failures are retried only on the server's
//! advice and some never. The runner already holds the retry budget, refuses failures that are not
//! transient, and refuses replays that could duplicate accepted output; this policy supplies
//! Codex's per-error decision and delay on top of it.
//!
//! Codex also retries failed connections without limit, on a separate counter with a 5 to 60
//! second delay, when `unbounded_connection_retries` is enabled. The runner counts every attempt
//! against one budget, so that mode is not ported here.

use std::sync::Arc;
use std::time::Duration;

use ra_core::model::{
    ModelRetryPolicy, ModelRetrySettings, NormalizedProviderError, RetryAdvice, RetryDecision,
    RetryPolicyContext,
};
use rand::Rng;

const INITIAL_DELAY_MS: f64 = 200.0;
const BACKOFF_FACTOR: f64 = 2.0;

/// Error codes Codex maps to failures that waiting does not fix: usage and quota limits, and
/// unavailable flex processing.
const NON_RETRYABLE_CODES: &[&str] = &[
    "usage_limit_reached",
    "usage_not_included",
    "insufficient_quota",
    "credit_balance_exhausted",
    "organization_spend_limit_exceeded",
    "project_spend_limit_exceeded",
    "organization_usage_limit_exceeded",
    "flex_unavailable",
];

/// Retry settings for product sampling requests with the provider's `stream_max_retries`.
pub(super) fn sampling_retry(max_retries: u32) -> ModelRetrySettings {
    ModelRetrySettings::new()
        .with_max_retries(max_retries)
        .with_policy(Arc::new(SamplingRetryPolicy))
}

#[derive(Debug)]
struct SamplingRetryPolicy;

#[async_trait::async_trait]
impl ModelRetryPolicy for SamplingRetryPolicy {
    async fn evaluate(&self, context: &RetryPolicyContext<'_>) -> RetryDecision {
        match retry_delay(context) {
            Some(delay) => RetryDecision::retry()
                .with_delay(delay)
                .with_reason("stream disconnected - retrying sampling request"),
            None => RetryDecision::no_retry(),
        }
    }
}

/// Codex's `CodexErr::retry_delay` for the failures that reach a retry policy.
fn retry_delay(context: &RetryPolicyContext<'_>) -> Option<Duration> {
    let normalized = context.normalized();
    let code = normalized.and_then(NormalizedProviderError::error_code);
    if code.is_some_and(|code| NON_RETRYABLE_CODES.contains(&code))
        || matches!(
            ra_model::openai::error_type(context.error()),
            Some("usage_limit_reached" | "usage_not_included" | "insufficient_quota")
        )
    {
        return None;
    }
    let server_delay = normalized
        .and_then(NormalizedProviderError::retry_after)
        .or_else(|| context.provider_advice().and_then(RetryAdvice::retry_after));
    // An overloaded server, and a 429 that is not a usage limit (Codex's `RetryLimit`), are
    // retried only when the server says when to come back.
    let server_only = code == Some("server_is_overloaded")
        || normalized.and_then(NormalizedProviderError::status_code) == Some(429);
    if server_only {
        return server_delay;
    }
    Some(server_delay.unwrap_or_else(|| backoff(u64::from(context.attempt()) + 1)))
}

/// Codex's retry backoff: 200 ms, doubling on each later retry, with up to ten percent jitter.
/// Retries zero and one both use the initial delay.
fn backoff(retry: u64) -> Duration {
    let exponent = i32::try_from(retry.saturating_sub(1)).unwrap_or(i32::MAX);
    let base = INITIAL_DELAY_MS * BACKOFF_FACTOR.powi(exponent);
    let jitter = rand::rng().random_range(0.9..1.1);
    Duration::from_secs_f64((base * jitter / 1000.0).min(Duration::MAX.as_secs_f64()))
}
