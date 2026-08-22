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

use std::time::Duration;

use super::RetryBackoffSettings;

/// Delay before the first retry when no layer configured one.
pub const DEFAULT_INITIAL_DELAY: Duration = Duration::from_millis(500);
/// Ceiling on the computed delay when no layer configured one.
pub const DEFAULT_MAX_DELAY: Duration = Duration::from_secs(8);
/// Growth factor per attempt when no layer configured one.
pub const DEFAULT_MULTIPLIER: f64 = 2.0;

/// How much of the delay jitter may remove.
///
/// The sampled factor lands in `[0.75, 1.0]`: jitter spreads a thundering herd apart without ever
/// making a client wait materially less than the schedule says. Delaying *more* is not an option
/// either — a ceiling that the jitter can exceed is not a ceiling.
pub const JITTER_RANGE: f64 = 0.25;

/// The longest endpoint-requested delay that is honored verbatim.
///
/// Beyond it the computed backoff is used instead. An endpoint asking for an hour is not describing
/// this request any more; honoring it would park the run in a sleep no budget accounts for and no
/// user can see, whereas falling back to bounded backoff fails the attempt quickly and lets the
/// retry budget end the run in the open.
pub const MAX_HONORED_RETRY_AFTER: Duration = Duration::from_secs(60);

/// A uniform random sample in `[0, 1]`, used to jitter one delay.
///
/// A newtype rather than a bare `f64` because the two obvious mistakes — passing a value on the
/// wrong scale, or passing the delay itself — both compile and both silently produce a schedule
/// nobody intended.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct JitterSample(f64);

impl JitterSample {
    /// The sample that removes nothing, leaving the computed delay exactly as scheduled.
    pub const ZERO: Self = Self(0.0);

    /// Clamps any value into the unit interval.
    ///
    /// Out-of-range and non-finite inputs are clamped rather than rejected: a generator that
    /// misbehaves should cost a slightly wrong delay, not a failed run.
    #[must_use]
    pub fn new(sample: f64) -> Self {
        if sample.is_nan() {
            return Self::ZERO;
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
        Self::ZERO
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
    /// A multiplier below `1.0` is raised to it. Configuration that shrinks the delay on every
    /// attempt is not slow backoff, it is a loop that tightens under exactly the load it was meant
    /// to relieve; refusing the call instead would fail a run over a number that has a sane reading.
    /// A non-finite multiplier falls back to the default for the same reason.
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
        if let Some(multiplier) = settings.multiplier() {
            resolved.multiplier = if multiplier.is_finite() {
                multiplier.max(1.0)
            } else {
                DEFAULT_MULTIPLIER
            };
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
    /// `attempt` is zero-based and counts the attempts already made, so `0` is the wait between the
    /// original request and the first retry.
    #[must_use]
    pub fn base_delay(&self, attempt: u32) -> Duration {
        let exponent = i32::try_from(attempt).unwrap_or(i32::MAX);
        let seconds = self.initial_delay.as_secs_f64() * self.multiplier.powi(exponent);
        if !seconds.is_finite() {
            return self.max_delay;
        }
        let capped = seconds.min(self.max_delay.as_secs_f64());
        Duration::try_from_secs_f64(capped).unwrap_or(self.max_delay)
    }

    /// The delay to actually wait before retrying, given what the endpoint asked for.
    ///
    /// A usable `retry_after` wins outright and is **not** jittered: it is an instruction about
    /// this endpoint's own state, not a guess that needs spreading out, and shortening it is how a
    /// client turns one rate-limit response into two. Anything else falls back to the jittered
    /// schedule.
    #[must_use]
    pub fn delay(
        &self,
        attempt: u32,
        retry_after: Option<Duration>,
        sample: JitterSample,
    ) -> Duration {
        if let Some(retry_after) = retry_after
            && retry_after > Duration::ZERO
            && retry_after <= MAX_HONORED_RETRY_AFTER
        {
            return retry_after;
        }
        let base = self.base_delay(attempt);
        if !self.jitter {
            return base;
        }
        base.mul_f64(1.0 - JITTER_RANGE * sample.get())
    }
}

impl Default for RetryBackoff {
    fn default() -> Self {
        Self::new()
    }
}
