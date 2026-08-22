//! Reading retry facts off an endpoint, and turning them into provider advice.
//!
//! Two jobs, both provider-neutral because both are about HTTP rather than about any one vendor:
//! parsing the headers an endpoint uses to ask for a delay or to state whether an attempt is worth
//! repeating, and deriving [`RetryAdvice`] from a failure that has already been normalized.
//!
//! # Advice is evidence, not a decision
//!
//! What is produced here says what the endpoint reported and what this adapter would suggest. It
//! deliberately does not net out to a yes: a suggestion to retry and an unsafe replay verdict
//! travel as two separate facts, because the layer that must reconcile them is the one holding the
//! attempt count and the budget. Folding them together here would silently move that decision into
//! a place that cannot see either.

use std::time::{Duration, SystemTime};

use ra_core::{
    error::Error,
    model::{
        ConversationContinuation, ModelRetryAdviceRequest, NormalizedProviderError, ReplaySafety,
        RetryAdvice,
    },
};

/// Header naming a delay in milliseconds, which some endpoints prefer for sub-second precision.
pub const RETRY_AFTER_MS_HEADER: &str = "retry-after-ms";
/// The standard header naming a delay in seconds.
pub const RETRY_AFTER_HEADER: &str = "retry-after";
/// Header through which an endpoint states, outright, whether an attempt is worth repeating.
pub const SHOULD_RETRY_HEADER: &str = "x-should-retry";

/// Advice reason recorded when the endpoint itself stated the verdict.
pub const REASON_ENDPOINT_VERDICT: &str = "endpoint_should_retry_header";
/// Advice reason recorded when the verdict follows from how the failure was classified.
pub const REASON_ERROR_CLASS: &str = "error_class";

/// What an endpoint's response headers say about retrying it.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RetryHints {
    retry_after: Option<Duration>,
    should_retry: Option<bool>,
}

impl RetryHints {
    /// Reads the hints from a header lookup.
    ///
    /// A lookup closure rather than a concrete header map: this crate's HTTP client is behind a
    /// feature flag, and a parser that only exists when that feature is on is a parser the compat
    /// layer and future transports cannot reuse.
    #[must_use]
    pub fn from_headers<'a>(lookup: impl Fn(&str) -> Option<&'a str>) -> Self {
        // Milliseconds first: an endpoint that sends both means the coarse one to be the fallback
        // for clients that do not know the precise one.
        let retry_after = lookup(RETRY_AFTER_MS_HEADER)
            .and_then(parse_millis)
            .or_else(|| lookup(RETRY_AFTER_HEADER).and_then(parse_seconds));
        Self {
            retry_after,
            should_retry: lookup(SHOULD_RETRY_HEADER).and_then(parse_bool),
        }
    }

    /// The delay the endpoint asked for.
    #[must_use]
    pub const fn retry_after(self) -> Option<Duration> {
        self.retry_after
    }

    /// The endpoint's own verdict on retrying.
    #[must_use]
    pub const fn should_retry(self) -> Option<bool> {
        self.should_retry
    }
}

/// Derives provider advice for a failed attempt.
///
/// Returns `None` for anything that is not a provider failure. A request rejected while it was
/// being built failed inside this process; answering with provider advice would dress a code defect
/// up as a transient condition, and the retry tier the taxonomy already assigns it is the better
/// answer.
#[must_use]
pub fn retry_advice(request: &ModelRetryAdviceRequest<'_>) -> Option<RetryAdvice> {
    let error = request.error();
    if !matches!(error, Error::Provider { .. }) {
        return None;
    }
    let normalized = NormalizedProviderError::from_error(error);

    // The endpoint's own verdict outranks the classification: it knows things about its state that
    // a status code cannot express, in both directions — a 500 it will keep producing, and a 400 it
    // will not.
    let (suggested, reason) = match normalized.and_then(NormalizedProviderError::should_retry) {
        Some(verdict) => (verdict, REASON_ENDPOINT_VERDICT),
        None => (error.is_retryable(), REASON_ERROR_CLASS),
    };

    let mut advice = RetryAdvice::new()
        .with_suggested(suggested)
        .with_reason(reason)
        .with_replay_safety(replay_safety(request, normalized));
    if let Some(retry_after) = normalized.and_then(NormalizedProviderError::retry_after) {
        advice = advice.with_retry_after(retry_after);
    }
    Some(advice)
}

/// The best verdict a failure can be given when nothing has reached the consumer yet.
///
/// For a request that carries its own history, that verdict is [`ReplaySafety::Safe`]: nothing was
/// published, and the endpoint holds no state the request could have advanced before it failed.
///
/// A request continuing server-managed state gets [`ReplaySafety::Unknown`] instead, and the
/// distinction is the timeout: no response arrived, which is exactly the case where the endpoint may
/// have accepted the request and appended this turn to the stored conversation already. Replaying it
/// then duplicates the turn in state the client cannot inspect and did not write. The verdict is
/// withheld rather than denied — a policy may still retry on evidence of its own, but it has to
/// decide that, and this layer must not hand it a "safe" it cannot support.
#[must_use]
pub const fn unstarted_replay_safety(continuation: &ConversationContinuation) -> ReplaySafety {
    if continuation.is_server_managed() {
        ReplaySafety::Unknown
    } else {
        ReplaySafety::Safe
    }
}

/// Whether replaying this request can duplicate what a consumer already saw.
///
/// The stamp the adapter left is authoritative when there is one: it is the only layer that knows
/// what reached the consumer. Advice never upgrades it — an unstamped streaming failure stays
/// unknown, because an adapter that did not say what it emitted has not established that it emitted
/// nothing.
fn replay_safety(
    request: &ModelRetryAdviceRequest<'_>,
    normalized: Option<&NormalizedProviderError>,
) -> ReplaySafety {
    match normalized.map_or(
        ReplaySafety::Unknown,
        NormalizedProviderError::replay_safety,
    ) {
        ReplaySafety::Unknown if !request.is_streaming() => {
            unstarted_replay_safety(request.continuation())
        }
        stamped => stamped,
    }
}

/// Parses a millisecond count, rejecting anything that is not a usable delay.
fn parse_millis(value: &str) -> Option<Duration> {
    duration_from_millis(value.trim().parse::<f64>().ok()?)
}

/// Parses the two forms `Retry-After` is defined to carry: a second count, or a date.
///
/// The date form is resolved against this machine's clock, so the delay it yields inherits whatever
/// skew that clock has. It is still read rather than dropped — a wrong-by-seconds delay is closer to
/// what the endpoint asked for than ignoring the instruction altogether — and the ceiling on how
/// long a requested delay is honored keeps a badly skewed clock from parking the run.
///
/// A date that has already passed names no wait at all, so it reads as absent and the schedule
/// answers instead. That keeps one invariant on the field: a delay that is present is a delay worth
/// waiting.
fn parse_seconds(value: &str) -> Option<Duration> {
    let value = value.trim();
    if let Ok(seconds) = value.parse::<f64>() {
        return duration_from_millis(seconds * 1_000.0);
    }
    let deadline = httpdate::parse_http_date(value).ok()?;
    deadline
        .duration_since(SystemTime::now())
        .ok()
        .filter(|delay| !delay.is_zero())
}

fn duration_from_millis(millis: f64) -> Option<Duration> {
    if !millis.is_finite() || millis < 0.0 {
        return None;
    }
    Duration::try_from_secs_f64(millis / 1_000.0).ok()
}

/// Parses the two spellings this header is defined to carry, and nothing else.
fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}
