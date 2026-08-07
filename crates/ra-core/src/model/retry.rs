//! Retry configuration shared by model-setting resolution and the later retry runtime.
//!
//! R1-2b defines the serializable configuration surface. R1-3 adds the minimal provider-advice
//! contract required by [`super::Model`]. R1-9 will add normalized provider errors and runtime
//! policy decisions without changing the model trait.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::duration::option_millis;
use super::request::ConversationContinuation;
use crate::{
    compat::{SchemaVersion, Unknown},
    error::Error,
};

/// Current model-retry-settings schema version.
pub const MODEL_RETRY_SETTINGS_SCHEMA_VERSION: SchemaVersion = SchemaVersion::new(1);
/// Current retry-backoff-settings schema version.
pub const RETRY_BACKOFF_SETTINGS_SCHEMA_VERSION: SchemaVersion = SchemaVersion::new(1);

/// Provider assessment of whether replaying the same request is safe.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReplaySafety {
    /// The provider cannot determine replay safety.
    Unknown,
    /// Replaying the request cannot duplicate accepted state or side effects.
    Safe,
    /// The request may already have been accepted or emitted irreversible output.
    Unsafe,
}

/// Provider-specific retry guidance.
///
/// This is evidence for the runtime retry policy, not the final retry decision.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryAdvice {
    suggested: Option<bool>,
    retry_after: Option<Duration>,
    replay_safety: ReplaySafety,
    reason: Option<String>,
}

impl RetryAdvice {
    /// Creates advice with no recommendation and unknown replay safety.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            suggested: None,
            retry_after: None,
            replay_safety: ReplaySafety::Unknown,
            reason: None,
        }
    }

    /// Sets the provider's retry recommendation.
    #[must_use]
    pub const fn with_suggested(mut self, suggested: bool) -> Self {
        self.suggested = Some(suggested);
        self
    }

    /// Sets the provider-requested delay.
    #[must_use]
    pub const fn with_retry_after(mut self, retry_after: Duration) -> Self {
        self.retry_after = Some(retry_after);
        self
    }

    /// Sets replay-safety evidence.
    #[must_use]
    pub const fn with_replay_safety(mut self, replay_safety: ReplaySafety) -> Self {
        self.replay_safety = replay_safety;
        self
    }

    /// Adds a stable diagnostic reason.
    #[must_use]
    pub fn with_reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }

    /// Provider retry recommendation.
    #[must_use]
    pub const fn suggested(&self) -> Option<bool> {
        self.suggested
    }

    /// Provider-requested delay.
    #[must_use]
    pub const fn retry_after(&self) -> Option<Duration> {
        self.retry_after
    }

    /// Replay-safety evidence.
    #[must_use]
    pub const fn replay_safety(&self) -> ReplaySafety {
        self.replay_safety
    }

    /// Stable diagnostic reason.
    #[must_use]
    pub fn reason(&self) -> Option<&str> {
        self.reason.as_deref()
    }
}

impl Default for RetryAdvice {
    fn default() -> Self {
        Self::new()
    }
}

/// Context passed to a model when deriving provider retry advice.
#[non_exhaustive]
#[derive(Debug)]
pub struct ModelRetryAdviceRequest<'a> {
    error: &'a Error,
    attempt: u32,
    streaming: bool,
    continuation: &'a ConversationContinuation,
}

impl<'a> ModelRetryAdviceRequest<'a> {
    /// Creates retry-advice context for a failed model attempt.
    #[must_use]
    pub const fn new(
        error: &'a Error,
        attempt: u32,
        streaming: bool,
        continuation: &'a ConversationContinuation,
    ) -> Self {
        Self {
            error,
            attempt,
            streaming,
            continuation,
        }
    }

    /// Framework error for the failed attempt.
    #[must_use]
    pub const fn error(&self) -> &Error {
        self.error
    }

    /// Zero-based attempt number.
    #[must_use]
    pub const fn attempt(&self) -> u32 {
        self.attempt
    }

    /// Whether the failed call used the streaming entry point.
    #[must_use]
    pub const fn is_streaming(&self) -> bool {
        self.streaming
    }

    /// Server-managed continuation used by the failed request.
    #[must_use]
    pub const fn continuation(&self) -> &ConversationContinuation {
        self.continuation
    }
}

/// Runner-managed model retry settings.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelRetrySettings {
    schema_version: SchemaVersion,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_retries: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    backoff: Option<RetryBackoffSettings>,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl ModelRetrySettings {
    /// Creates an unset retry configuration.
    #[must_use]
    pub fn new() -> Self {
        Self {
            schema_version: MODEL_RETRY_SETTINGS_SCHEMA_VERSION,
            max_retries: None,
            backoff: None,
            unknown: Unknown::new(),
        }
    }

    /// Sets retries allowed after the initial request.
    #[must_use]
    pub const fn with_max_retries(mut self, max_retries: u32) -> Self {
        self.max_retries = Some(max_retries);
        self
    }

    /// Sets retry backoff configuration.
    #[must_use]
    pub fn with_backoff(mut self, backoff: RetryBackoffSettings) -> Self {
        self.backoff = Some(backoff);
        self
    }

    /// Schema version.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Retries allowed after the initial request.
    #[must_use]
    pub const fn max_retries(&self) -> Option<u32> {
        self.max_retries
    }

    /// Backoff configuration.
    #[must_use]
    pub const fn backoff(&self) -> Option<&RetryBackoffSettings> {
        self.backoff.as_ref()
    }

    /// Unknown fields retained during deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }

    /// Merges layers in increasing precedence, or `None` when no layer configured retries.
    pub(crate) fn merge<'a>(layers: impl IntoIterator<Item = Option<&'a Self>>) -> Option<Self> {
        let mut resolved: Option<Self> = None;
        for layer in layers.into_iter().flatten() {
            let target = resolved.get_or_insert_with(Self::new);
            override_if_some(&mut target.max_retries, layer.max_retries);
            target.backoff =
                RetryBackoffSettings::merge([target.backoff.as_ref(), layer.backoff.as_ref()]);
            // A layer written by a newer version carries its additions here. Dropping them would
            // make the merged value quietly less informative than the layer it came from.
            target.unknown.extend_from(&layer.unknown);
        }
        resolved
    }
}

impl Default for ModelRetrySettings {
    fn default() -> Self {
        Self::new()
    }
}

/// Exponential-backoff settings for model retries.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RetryBackoffSettings {
    schema_version: SchemaVersion,
    #[serde(with = "option_millis", skip_serializing_if = "Option::is_none")]
    initial_delay: Option<Duration>,
    #[serde(with = "option_millis", skip_serializing_if = "Option::is_none")]
    max_delay: Option<Duration>,
    #[serde(skip_serializing_if = "Option::is_none")]
    multiplier: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    jitter: Option<bool>,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl RetryBackoffSettings {
    /// Creates an unset backoff configuration.
    #[must_use]
    pub fn new() -> Self {
        Self {
            schema_version: RETRY_BACKOFF_SETTINGS_SCHEMA_VERSION,
            initial_delay: None,
            max_delay: None,
            multiplier: None,
            jitter: None,
            unknown: Unknown::new(),
        }
    }

    /// Sets the delay before the first retry.
    #[must_use]
    pub const fn with_initial_delay(mut self, delay: Duration) -> Self {
        self.initial_delay = Some(delay);
        self
    }

    /// Sets the maximum delay between retries.
    #[must_use]
    pub const fn with_max_delay(mut self, delay: Duration) -> Self {
        self.max_delay = Some(delay);
        self
    }

    /// Sets the exponential multiplier.
    #[must_use]
    pub const fn with_multiplier(mut self, multiplier: f64) -> Self {
        self.multiplier = Some(multiplier);
        self
    }

    /// Explicitly enables or disables jitter.
    #[must_use]
    pub const fn with_jitter(mut self, jitter: bool) -> Self {
        self.jitter = Some(jitter);
        self
    }

    /// Schema version.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Delay before the first retry.
    #[must_use]
    pub const fn initial_delay(&self) -> Option<Duration> {
        self.initial_delay
    }

    /// Maximum delay between retries.
    #[must_use]
    pub const fn max_delay(&self) -> Option<Duration> {
        self.max_delay
    }

    /// Exponential multiplier.
    #[must_use]
    pub const fn multiplier(&self) -> Option<f64> {
        self.multiplier
    }

    /// Explicit jitter setting.
    #[must_use]
    pub const fn jitter(&self) -> Option<bool> {
        self.jitter
    }

    /// Unknown fields retained during deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }

    fn merge<'a>(layers: impl IntoIterator<Item = Option<&'a Self>>) -> Option<Self> {
        let mut resolved: Option<Self> = None;
        for layer in layers.into_iter().flatten() {
            let target = resolved.get_or_insert_with(Self::new);
            override_if_some(&mut target.initial_delay, layer.initial_delay);
            override_if_some(&mut target.max_delay, layer.max_delay);
            override_if_some(&mut target.multiplier, layer.multiplier);
            override_if_some(&mut target.jitter, layer.jitter);
            target.unknown.extend_from(&layer.unknown);
        }
        resolved
    }
}

impl Default for RetryBackoffSettings {
    fn default() -> Self {
        Self::new()
    }
}

fn override_if_some<T: Copy>(target: &mut Option<T>, incoming: Option<T>) {
    if incoming.is_some() {
        *target = incoming;
    }
}
