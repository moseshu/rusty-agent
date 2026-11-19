//! Turning backoff configuration into the delay before one particular attempt.
//!
//! [`RetryBackoffSettings`] is the configuration surface: every field optional, every layer free to
//! leave it unset. [`RetryBackoff`] is what that resolves to — four concrete values plus the
//! arithmetic — so the question "how long before attempt 3" has one answer, computed in one place,
//! rather than being re-derived by each caller that happens to hold the settings.
//!
//! # Randomness is injected, never sourced here
//!
//! Jitter needs a random sample, and this crate produces none. The caller passes one in as a
//! [`JitterSample`]. That keeps the kernel deterministic — the same inputs give the same delay, so
//! a schedule can be asserted rather than approximated — and it leaves the choice of generator to
//! the layer that already owns the run's non-determinism.
//!
//! # The SDK's schedule
//!
//! The defaults, the jitter and the use of an endpoint's own delay follow the `OpenAI` Agents
//! SDK's `_default_retry_delay` and `_evaluate_retry`: the delay starts at a quarter of a second
//! and doubles up to two seconds, jitter scales it by `[0.875, 1.125]` and then the ceiling
//! applies, and a delay the endpoint asked for is waited out exactly, however short or long.

use std::time::Duration;

use super::RetryBackoffSettings;

/// Delay before the first retry when no layer configured one, as in the SDK.
pub const DEFAULT_INITIAL_DELAY: Duration = Duration::from_millis(250);
/// Ceiling on the computed delay when no layer configured one, as in the SDK.
pub const DEFAULT_MAX_DELAY: Duration = Duration::from_secs(2);
/// Growth factor per attempt when no layer configured one.
pub const DEFAULT_MULTIPLIER: f64 = 2.0;

/// The total width of the jitter factor, centered on the scheduled delay.
///
/// The sampled factor lands in `[0.875, 1.125]`, as in the SDK, and the ceiling applies after it,
/// so jitter never pushes a delay past [`RetryBackoff::max_delay`].
pub const JITTER_RANGE: f64 = 0.25;

/// A uniform random sample in `[0, 1]`, used to jitter one delay: `0` gives the shortest jittered
/// delay, [`Self::MIDPOINT`] the scheduled one and `1` the longest.
///
/// A newtype rather than a bare `f64` because the two obvious mistakes — passing a value on the
/// wrong scale, or passing the delay itself — both compile and both silently produce a schedule
/// nobody intended.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct JitterSample(f64);

impl JitterSample {
    /// The lowest sample, which shortens the delay the most.
    pub const ZERO: Self = Self(0.0);

    /// The sample that leaves the computed delay exactly as scheduled.
    pub const MIDPOINT: Self = Self(0.5);

    /// Clamps any value into the unit interval.
    ///
    /// Out-of-range and non-finite inputs are clamped rather than rejected: a generator that
    /// misbehaves should cost a slightly wrong delay, not a failed run.
    #[must_use]
    pub fn new(sample: f64) -> Self {
        if sample.is_nan() {
            return Self::MIDPOINT;
        }
        Self(sample.clamp(0.0, 1.0))
    }

    /// The clamped sample.
    #[must_use]
    pub const fn get(self) -> f64 {
        self.0
    }
}

impl Default for JitterSample {
    fn default() -> Self {
        Self::MIDPOINT
    }
}

/// Resolved exponential backoff: what the settings mean once every hole has been filled.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RetryBackoff {
    initial_delay: Duration,
    max_delay: Duration,
    multiplier: f64,
    jitter: bool,
}

impl RetryBackoff {
    /// The schedule used when nothing was configured.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            initial_delay: DEFAULT_INITIAL_DELAY,
            max_delay: DEFAULT_MAX_DELAY,
            multiplier: DEFAULT_MULTIPLIER,
            jitter: true,
        }
    }

    /// Resolves configuration, falling back to the defaults field by field.
    ///
    /// Any multiplier the SDK accepts is used as given: zero, a value below one, and infinity, which
    /// the ceiling bounds. The SDK rejects a negative or `NaN` multiplier when its settings are
    /// built; a Rust builder cannot fail there, so such a value is treated as unset and the default
    /// applies.
    #[must_use]
    pub fn from_settings(settings: Option<&RetryBackoffSettings>) -> Self {
        let mut resolved = Self::new();
        let Some(settings) = settings else {
            return resolved;
        };
        if let Some(initial_delay) = settings.initial_delay() {
            resolved.initial_delay = initial_delay;
        }
        if let Some(max_delay) = settings.max_delay() {
            resolved.max_delay = max_delay;
        }
        if let Some(multiplier) = settings.multiplier()
            && multiplier >= 0.0
        {
            resolved.multiplier = multiplier;
        }
        if let Some(jitter) = settings.jitter() {
            resolved.jitter = jitter;
        }
        resolved
    }

    /// Delay before the first retry.
    #[must_use]
    pub const fn initial_delay(&self) -> Duration {
        self.initial_delay
    }

    /// Ceiling on the computed delay.
    #[must_use]
    pub const fn max_delay(&self) -> Duration {
        self.max_delay
    }

    /// Growth factor per attempt.
    #[must_use]
    pub const fn multiplier(&self) -> f64 {
        self.multiplier
    }

    /// Whether the computed delay is jittered.
    #[must_use]
    pub const fn jitter(&self) -> bool {
        self.jitter
    }

    /// The scheduled delay before the retry that follows `attempt`, with no jitter applied.
    ///
    /// `attempt` is the failed attempt's budget number, as for
    /// [`RetryPolicyContext::attempt`](super::RetryPolicyContext::attempt): `1` is the wait between
    /// the original request and the first retry. As in the `OpenAI` Agents SDK, `0` is read as `1`.
    #[must_use]
    pub fn base_delay(&self, attempt: u32) -> Duration {
        let exponent = i32::try_from(attempt.saturating_sub(1)).unwrap_or(i32::MAX);
        let seconds = self.initial_delay.as_secs_f64() * self.multiplier.powi(exponent);
        if !seconds.is_finite() {
            return self.max_delay;
        }
        let capped = seconds.min(self.max_delay.as_secs_f64());
        Duration::try_from_secs_f64(capped).unwrap_or(self.max_delay)
    }

    /// The delay to actually wait before retrying, given what the endpoint asked for.
    ///
    /// A `retry_after` wins outright and is **not** jittered or capped, as in the SDK: it is an
    /// instruction about this endpoint's own state, not a guess that needs spreading out, and
    /// shortening it is how a client turns one rate-limit response into two. Zero means retry at
    /// once. Without one, the jittered schedule applies.
    #[must_use]
    pub fn delay(
        &self,
        attempt: u32,
        retry_after: Option<Duration>,
        sample: JitterSample,
    ) -> Duration {
        if let Some(retry_after) = retry_after {
            return retry_after;
        }
        let base = self.base_delay(attempt);
        if !self.jitter {
            return base;
        }
        let factor = 1.0 - JITTER_RANGE / 2.0 + JITTER_RANGE * sample.get();
        let seconds = (base.as_secs_f64() * factor).min(self.max_delay.as_secs_f64());
        // Cap before conversion: jitter can exceed Duration's range, and the floating-point
        // representation of the ceiling can itself round beyond that range or the exact ceiling.
        Duration::try_from_secs_f64(seconds)
            .unwrap_or(self.max_delay)
            .min(self.max_delay)
    }
}

impl Default for RetryBackoff {
    fn default() -> Self {
        Self::new()
    }
}
