//! Normalized provider failures and the backoff schedule they feed.
//!
//! The two halves are asserted against the same criterion: a retry policy must be able to reach
//! every fact it needs by field, and must never be able to reach a fact nobody established.

use std::{error::Error as StdError, io, time::Duration};

use ra_core::{
    error::{Error, ProviderErrorKind, Recoverability},
    model::{
        JitterSample, NormalizedProviderError, ReplaySafety, RetryBackoff, RetryBackoffSettings,
        replay_safety_of, stamp_replay_safety,
    },
};

fn rate_limited() -> NormalizedProviderError {
    NormalizedProviderError::new(ProviderErrorKind::RateLimit, "OpenAI HTTP 429: slow down")
        .with_status_code(429)
        .with_error_code("rate_limit_exceeded")
        .with_request_id("req_abc")
        .with_retry_after(Duration::from_secs(2))
}

/// The record travels inside the error rather than beside it, so no layer in between has to know
/// it exists in order not to drop it.
#[test]
fn every_recorded_fact_is_reachable_from_the_error_it_produced() {
    let error = rate_limited().into_error();

    assert_eq!(error.code(), "provider.rate_limit");
    assert_eq!(error.recoverability(), Recoverability::Retryable);

    let facts = NormalizedProviderError::from_error(&error).expect("facts should survive");
    assert_eq!(facts.kind(), ProviderErrorKind::RateLimit);
    assert_eq!(facts.status_code(), Some(429));
    assert_eq!(facts.error_code(), Some("rate_limit_exceeded"));
    assert_eq!(facts.request_id(), Some("req_abc"));
    assert_eq!(facts.retry_after(), Some(Duration::from_secs(2)));
    assert_eq!(facts.message(), "OpenAI HTTP 429: slow down");
    assert!(
        error.to_string().contains(facts.message()),
        "the error and its facts must state the same failure"
    );
}

/// Nothing asked and nothing answered are the same state here, and both must read as unknown: a
/// policy that treats silence about replay safety as approval replays the streams that must not be.
#[test]
fn an_error_that_carries_no_facts_admits_it() {
    let error = Error::provider(ProviderErrorKind::ServerError, "gateway fell over");

    assert!(NormalizedProviderError::from_error(&error).is_none());
    assert_eq!(replay_safety_of(&error), ReplaySafety::Unknown);
    assert_eq!(rate_limited().should_retry(), None);
    assert_eq!(rate_limited().replay_safety(), ReplaySafety::Unknown);
}

/// Normalization inserts a link into the source chain; it must not cut the chain.
#[test]
fn the_underlying_failure_stays_reachable_below_the_facts() {
    let cause = io::Error::new(io::ErrorKind::ConnectionReset, "connection reset by peer");
    let error = NormalizedProviderError::new(ProviderErrorKind::Network, "transport failed")
        .with_source(cause)
        .into_error();

    let mut chain = Vec::new();
    let mut source = StdError::source(&error);
    while let Some(current) = source {
        chain.push(current.to_string());
        source = current.source();
    }

    assert_eq!(chain.len(), 2, "facts then cause: {chain:?}");
    assert!(chain[1].contains("connection reset by peer"));
}

/// Two fields that can disagree eventually do, so these are projections rather than storage.
#[test]
fn timeout_and_network_are_read_off_the_classification() {
    let timeout = NormalizedProviderError::new(ProviderErrorKind::Timeout, "timed out");
    assert!(timeout.is_timeout());
    assert!(
        !timeout.is_network_error(),
        "a timeout may well have been received and acted on"
    );

    let network = NormalizedProviderError::new(ProviderErrorKind::Network, "no route");
    assert!(network.is_network_error());
    assert!(!network.is_timeout());

    let rate_limit = rate_limited();
    assert!(!rate_limit.is_timeout());
    assert!(!rate_limit.is_network_error());
}

/// The stamp is applied by a layer that knows only one thing the adapter did not; everything the
/// adapter did establish has to come through it intact.
#[test]
fn stamping_replay_safety_changes_the_verdict_and_nothing_else() {
    let stamped = stamp_replay_safety(rate_limited().into_error(), ReplaySafety::Unsafe);

    let facts = NormalizedProviderError::from_error(&stamped).expect("facts should survive");
    assert_eq!(facts.replay_safety(), ReplaySafety::Unsafe);
    assert_eq!(facts.status_code(), Some(429));
    assert_eq!(facts.error_code(), Some("rate_limit_exceeded"));
    assert_eq!(facts.request_id(), Some("req_abc"));
    assert_eq!(facts.retry_after(), Some(Duration::from_secs(2)));
    assert_eq!(stamped.code(), "provider.rate_limit");
}

/// A provider error built the plain way still has to end up carrying the verdict, and the cause it
/// was already holding must not be dropped to make room for it.
#[test]
fn stamping_an_unnormalized_provider_error_normalizes_it_in_place() {
    let cause = io::Error::other("stream cut");
    let stamped = stamp_replay_safety(
        Error::provider(ProviderErrorKind::Network, "stream ended early").with_source(cause),
        ReplaySafety::Unsafe,
    );

    assert_eq!(
        stamped.to_string(),
        "provider 错误（Network）：stream ended early"
    );
    let facts = NormalizedProviderError::from_error(&stamped).expect("facts should be created");
    assert_eq!(facts.replay_safety(), ReplaySafety::Unsafe);
    assert!(
        facts.source().expect("cause should survive").to_string() == "stream cut",
        "the original cause must stay in the chain"
    );
}

/// A cancellation is not a provider failure. Dressing one up with provider facts would give a
/// policy a retryable-looking error for a run the user deliberately stopped.
#[test]
fn stamping_leaves_errors_from_other_subsystems_alone() {
    let stamped = stamp_replay_safety(Error::cancelled("用户中断"), ReplaySafety::Safe);

    assert!(stamped.is_cancelled());
    assert_eq!(stamped.recoverability(), Recoverability::Cancelled);
    assert!(NormalizedProviderError::from_error(&stamped).is_none());

    let caller = stamp_replay_safety(Error::caller("tool schema is invalid"), ReplaySafety::Safe);
    assert_eq!(caller.recoverability(), Recoverability::Fatal);
    assert!(NormalizedProviderError::from_error(&caller).is_none());
}

/// The SDK's default schedule: a quarter of a second before the first retry, doubling up to two
/// seconds. Attempt one is the wait before the first retry, and attempt zero reads as one. The
/// ceiling is a ceiling: an exponent that would overflow it produces the ceiling, not an unbounded
/// sleep.
#[test]
fn the_schedule_grows_by_the_multiplier_until_it_reaches_the_ceiling() {
    let backoff = RetryBackoff::new();

    let schedule: Vec<Duration> = (1..=5).map(|attempt| backoff.base_delay(attempt)).collect();
    assert_eq!(
        schedule,
        vec![
            Duration::from_millis(250),
            Duration::from_millis(500),
            Duration::from_secs(1),
            Duration::from_secs(2),
            Duration::from_secs(2),
        ]
    );
    assert_eq!(backoff.base_delay(0), backoff.base_delay(1));
    assert_eq!(backoff.base_delay(u32::MAX), Duration::from_secs(2));
}

/// Configuration is optional field by field. As in the SDK, any non-negative multiplier is used as
/// given, including zero, a shrinking one and infinity, which the ceiling bounds; a negative or
/// `NaN` multiplier, which the SDK rejects when its settings are built, is treated as unset.
#[test]
fn settings_fill_only_the_fields_they_set_and_accept_the_sdk_multipliers() {
    assert_eq!(RetryBackoff::from_settings(None), RetryBackoff::new());

    let sparse = RetryBackoffSettings::new().with_initial_delay(Duration::from_millis(100));
    let resolved = RetryBackoff::from_settings(Some(&sparse));
    assert_eq!(resolved.initial_delay(), Duration::from_millis(100));
    assert_eq!(resolved.max_delay(), RetryBackoff::new().max_delay());
    assert_eq!(resolved.multiplier(), RetryBackoff::new().multiplier());
    assert!(resolved.jitter());

    let schedule = |multiplier: f64| {
        let backoff = RetryBackoff::from_settings(Some(
            &RetryBackoffSettings::new()
                .with_initial_delay(Duration::from_secs(1))
                .with_max_delay(Duration::from_secs(10))
                .with_multiplier(multiplier)
                .with_jitter(false),
        ));
        (1..=3)
            .map(|attempt| backoff.base_delay(attempt).as_secs_f64())
            .collect::<Vec<_>>()
    };
    assert_eq!(schedule(0.0), [1.0, 0.0, 0.0]);
    assert_eq!(schedule(0.5), [1.0, 0.5, 0.25]);
    assert_eq!(schedule(f64::INFINITY), [1.0, 10.0, 10.0]);

    for rejected in [f64::NAN, -1.0] {
        let settings = RetryBackoffSettings::new().with_multiplier(rejected);
        assert_eq!(
            RetryBackoff::from_settings(Some(&settings)).multiplier(),
            2.0
        );
    }

    let inverted = RetryBackoff::from_settings(Some(
        &RetryBackoffSettings::new()
            .with_initial_delay(Duration::from_secs(5))
            .with_max_delay(Duration::from_secs(2))
            .with_jitter(false),
    ));
    assert_eq!(inverted.base_delay(1), Duration::from_secs(2));
}

/// As in the SDK, jitter scales the scheduled delay by `[0.875, 1.125]` and the ceiling applies
/// afterwards, so it never pushes a delay past the ceiling; with jitter off the schedule is exact.
#[test]
fn jitter_spreads_the_delay_around_the_schedule_within_the_ceiling() {
    let backoff = RetryBackoff::new();

    assert_eq!(
        backoff.delay(3, None, JitterSample::ZERO),
        Duration::from_millis(875)
    );
    assert_eq!(
        backoff.delay(3, None, JitterSample::MIDPOINT),
        Duration::from_secs(1)
    );
    assert_eq!(
        backoff.delay(3, None, JitterSample::new(1.0)),
        Duration::from_millis(1_125)
    );
    assert_eq!(
        backoff.delay(3, None, JitterSample::new(f64::NAN)),
        Duration::from_secs(1),
        "a misbehaving generator costs a slightly wrong delay, never a failed run"
    );
    assert_eq!(JitterSample::default(), JitterSample::MIDPOINT);

    // At the ceiling, the longest jittered delay is the ceiling itself.
    assert_eq!(
        backoff.delay(4, None, JitterSample::new(1.0)),
        Duration::from_secs(2)
    );
    assert_eq!(
        backoff.delay(4, None, JitterSample::ZERO),
        Duration::from_millis(1_750)
    );

    let steady = RetryBackoff::from_settings(Some(&RetryBackoffSettings::new().with_jitter(false)));
    assert_eq!(
        steady.delay(3, None, JitterSample::new(1.0)),
        Duration::from_secs(1)
    );
}

/// Jitter is capped before conversion to a duration, even when the uncapped result would overflow.
#[test]
fn jitter_caps_large_delays_without_overflowing() {
    for (initial_delay, max_delay) in [
        (
            Duration::from_secs(17_000_000_000_000_000_000),
            Duration::from_secs(18_000_000_000_000_000_000),
        ),
        (Duration::MAX, Duration::MAX),
        (
            Duration::from_secs((1_u64 << 54) - 1),
            Duration::from_secs((1_u64 << 54) - 1),
        ),
    ] {
        let backoff = RetryBackoff::from_settings(Some(
            &RetryBackoffSettings::new()
                .with_initial_delay(initial_delay)
                .with_max_delay(max_delay),
        ));
        assert_eq!(backoff.delay(1, None, JitterSample::new(1.0)), max_delay);
        assert!(backoff.delay(1, None, JitterSample::ZERO) < initial_delay);
    }
}

/// The endpoint's own delay is an instruction about its state, not a guess that needs spreading
/// out. As in the SDK it is waited out exactly, however long, and zero means retry at once.
#[test]
fn an_endpoint_delay_is_waited_out_exactly() {
    let backoff = RetryBackoff::new();

    assert_eq!(
        backoff.delay(1, Some(Duration::from_secs(30)), JitterSample::new(1.0)),
        Duration::from_secs(30),
        "an honored delay is not jittered"
    );
    assert_eq!(
        backoff.delay(2, Some(Duration::from_secs(3600)), JitterSample::ZERO),
        Duration::from_secs(3600),
        "nor capped by the ceiling"
    );
    assert_eq!(
        backoff.delay(2, Some(Duration::ZERO), JitterSample::ZERO),
        Duration::ZERO
    );
    assert_eq!(
        backoff.delay(2, None, JitterSample::MIDPOINT),
        Duration::from_millis(500)
    );
}
