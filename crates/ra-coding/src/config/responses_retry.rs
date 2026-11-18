// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Turn-level retry of product sampling requests.
//!
//! Codex retries a failed sampling stream from its turn loop, up to the provider's
//! `stream_max_retries`, when the error yields a retry delay: the server's advised delay when one
//! was sent, or an exponential backoff otherwise. Some failures are retried only on the server's
//! advice and some never. The runner holds the retry budget and refuses replays that could
//! duplicate accepted output; this policy supplies
//! Codex's per-error decision and delay on top of it.
//!
//! Codex also retries failed connections without limit, on a separate counter with a 5 to 60
//! second delay, when `unbounded_connection_retries` is enabled. The runner counts every attempt
//! against one budget, so that mode is not ported here.

use std::sync::Arc;
use std::time::Duration;

use ra_core::error::{Error, ProviderErrorKind};
use ra_core::model::{
    ModelRetryPolicy, ModelRetrySettings, NormalizedProviderError, RetryAdvice, RetryDecision,
    RetryPolicyContext,
};
use rand::Rng;

const INITIAL_DELAY_MS: f64 = 200.0;
const BACKOFF_FACTOR: f64 = 2.0;

/// Quota codes recognized on HTTP 429, independently of the error type.
const HTTP_QUOTA_CODES: &[&str] = &[
    "insufficient_quota",
    "credit_balance_exhausted",
    "organization_spend_limit_exceeded",
    "project_spend_limit_exceeded",
    "organization_usage_limit_exceeded",
];

/// Terminal codes recognized by Codex's `parse_failed_response`, rather than its HTTP mapper.
const FAILED_RESPONSE_TERMINAL_CODES: &[&str] = &[
    "context_length_exceeded",
    "insufficient_quota",
    "credit_balance_exhausted",
    "organization_spend_limit_exceeded",
    "project_spend_limit_exceeded",
    "usage_not_included",
    "cyber_policy",
    "bio_policy",
    "misalignment_policy_violation",
    "invalid_prompt",
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
    let error = context.error();
    let status = normalized.and_then(NormalizedProviderError::status_code);
    let event = ra_model::openai::response_failure_event(error);
    // Codex reads a stream frame's code only from its nested `error` object.
    let code = if status.is_some() {
        ra_model::openai::error_code(error)
    } else if event.is_some() {
        ra_model::openai::response_stream_error_code(error)
    } else {
        normalized.and_then(NormalizedProviderError::error_code)
    };
    let server_delay = normalized
        .and_then(NormalizedProviderError::retry_after)
        .or_else(|| context.provider_advice().and_then(RetryAdvice::retry_after));
    match status {
        Some(400) => return None,
        Some(429) => {
            if code
                .is_some_and(|code| HTTP_QUOTA_CODES.contains(&code) || code == "flex_unavailable")
                || matches!(
                    ra_model::openai::error_type(error),
                    Some("usage_limit_reached" | "usage_not_included" | "insufficient_quota")
                )
            {
                return None;
            }
            return server_delay;
        }
        Some(503) if code == Some("server_is_overloaded") => return server_delay,
        Some(403) if code == Some("misalignment_policy_violation") => return None,
        Some(_) => {}
        None if event == Some("response.failed") => {
            if code.is_some_and(|code| FAILED_RESPONSE_TERMINAL_CODES.contains(&code)) {
                return None;
            }
            if code == Some("server_is_overloaded") {
                return server_delay;
            }
        }
        // Codex's `process_responses_event` stops on an `error` frame's flex refusal. It does not
        // inspect `response.error`, whose stream then ends and is retried.
        None if event == Some("error") && code == Some("flex_unavailable") => return None,
        None if event == Some("response.incomplete") => {
            if ra_model::openai::response_incomplete_reason(error) != Some("content_filter") {
                return None;
            }
        }
        None => {
            // Local caller/configuration failures and gateway sign-in cannot be repaired by
            // replay. Transport and stream decoder failures can, even when classified Behavior.
            match error {
                Error::Provider {
                    kind:
                        ProviderErrorKind::Network
                        | ProviderErrorKind::Timeout
                        | ProviderErrorKind::ServerError
                        | ProviderErrorKind::RateLimit
                        | ProviderErrorKind::Conflict
                        | ProviderErrorKind::Behavior,
                    ..
                } => {}
                _ => return None,
            }
        }
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

/// Codex hides the first WebSocket reconnect notification only in release builds.
pub(super) const fn report_retry(retry: u32, debug: bool, websocket: bool) -> bool {
    retry > 1 || debug || !websocket
}

/// External tests exercise the actual product policy and its jittered backoff.
#[cfg(feature = "test-api")]
#[doc(hidden)]
pub mod test_api {
    /// The product delay for one failed attempt, before the runtime enforces replay safety.
    #[must_use]
    pub fn delay(error: &ra_core::error::Error, attempt: u32) -> Option<std::time::Duration> {
        super::retry_delay(&ra_core::model::RetryPolicyContext::new(
            error, attempt, 5, true, None,
        ))
    }

    /// Whether Codex would surface a reconnect notification for these build and transport flags.
    #[must_use]
    pub const fn reports_retry(retry: u32, debug: bool, websocket: bool) -> bool {
        super::report_retry(retry, debug, websocket)
    }
}
