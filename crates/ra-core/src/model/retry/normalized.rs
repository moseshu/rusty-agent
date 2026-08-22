//! Provider failure facts, normalized once at the adapter boundary.
//!
//! # Why the taxonomy is not enough on its own
//!
//! [`Error::Provider`] answers "which layer failed and what to do
//! about it". A retry policy needs three things it cannot answer: how long the endpoint asked to be
//! left alone, which request to quote when the failure has to be reported, and whether replaying is
//! permissible at all. Today an adapter formats those into the message — which leaves the policy
//! parsing prose, the one thing the error taxonomy exists to forbid.
//!
//! So the adapter records them as fields and attaches the record as the error's source. The error
//! keeps its shape, every layer above keeps reading `recoverability()`, and a policy that wants the
//! detail asks for it by type through [`NormalizedProviderError::from_error`].
//!
//! # What is deliberately not a field here
//!
//! **An abort flag.** A cancelled call is not a provider failure in this framework: it produces
//! [`Error::Cancelled`], and its recoverability is
//! [`Cancelled`](crate::error::Recoverability::Cancelled) rather than any retryable tier. Carrying
//! `is_abort` alongside the provider facts would create a second, weaker way to ask the same
//! question, and the weak one would eventually be used to retry a run the user stopped.
//!
//! **Stored `is_timeout` / `is_network_error` flags.** Both are projections of
//! [`ProviderErrorKind`], derived here for the same reason recoverability is derived rather than
//! stored: two fields that can disagree eventually do.

use std::{error::Error as StdError, fmt, time::Duration};

use super::ReplaySafety;
use crate::error::{BoxError, Error, ProviderErrorKind};

/// One provider failure, reduced to the facts a retry policy reads.
///
/// Construct it in the adapter that saw the wire response, then call [`Self::into_error`]: the
/// resulting [`Error`] carries these facts in its source chain, so nothing between the adapter and
/// the policy has to know they are there.
#[non_exhaustive]
#[derive(Debug)]
pub struct NormalizedProviderError {
    kind: ProviderErrorKind,
    message: String,
    status_code: Option<u16>,
    error_code: Option<String>,
    request_id: Option<String>,
    retry_after: Option<Duration>,
    should_retry: Option<bool>,
    replay_safety: ReplaySafety,
    source: Option<BoxError>,
}

impl NormalizedProviderError {
    /// Creates a record carrying only the classification and the developer-facing message.
    ///
    /// Everything else is absent until an adapter reports it. Absent means "the endpoint did not
    /// say", never "the endpoint said no" — a policy that treats a missing `retry_after` as zero
    /// and a missing replay verdict as safe is the failure mode this distinction exists to prevent,
    /// which is why [`ReplaySafety::Unknown`] is a state of its own.
    #[must_use]
    pub fn new(kind: ProviderErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            status_code: None,
            error_code: None,
            request_id: None,
            retry_after: None,
            should_retry: None,
            replay_safety: ReplaySafety::Unknown,
            source: None,
        }
    }

    /// Records the HTTP status the endpoint answered with.
    #[must_use]
    pub const fn with_status_code(mut self, status_code: u16) -> Self {
        self.status_code = Some(status_code);
        self
    }

    /// Records the vendor's own error code, verbatim.
    ///
    /// Kept as an open string rather than mapped into the neutral kinds: a code such as
    /// `insufficient_quota` distinguishes "wait and it clears" from "nobody is going to pay for the
    /// next attempt", and that distinction is a policy decision this layer must not pre-empt by
    /// flattening the code away.
    #[must_use]
    pub fn with_error_code(mut self, error_code: impl Into<String>) -> Self {
        self.error_code = Some(error_code.into());
        self
    }

    /// Records the diagnostic identifier of the failed request.
    #[must_use]
    pub fn with_request_id(mut self, request_id: impl Into<String>) -> Self {
        self.request_id = Some(request_id.into());
        self
    }

    /// Records the delay the endpoint asked for.
    #[must_use]
    pub const fn with_retry_after(mut self, retry_after: Duration) -> Self {
        self.retry_after = Some(retry_after);
        self
    }

    /// Records an endpoint that stated outright whether this failure is worth retrying.
    #[must_use]
    pub const fn with_should_retry(mut self, should_retry: bool) -> Self {
        self.should_retry = Some(should_retry);
        self
    }

    /// Records whether replaying the request can duplicate what a consumer already saw.
    #[must_use]
    pub const fn with_replay_safety(mut self, replay_safety: ReplaySafety) -> Self {
        self.replay_safety = replay_safety;
        self
    }

    /// Attaches the underlying error this record was normalized from.
    #[must_use]
    pub fn with_source(mut self, source: impl Into<BoxError>) -> Self {
        self.source = Some(source.into());
        self
    }

    /// Neutral classification, which decides recoverability.
    #[must_use]
    pub const fn kind(&self) -> ProviderErrorKind {
        self.kind
    }

    /// Developer-facing description, identical to the one on the error it produces.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// HTTP status, when the failure got as far as a response.
    #[must_use]
    pub const fn status_code(&self) -> Option<u16> {
        self.status_code
    }

    /// The vendor's own error code.
    #[must_use]
    pub fn error_code(&self) -> Option<&str> {
        self.error_code.as_deref()
    }

    /// Diagnostic identifier of the failed request.
    #[must_use]
    pub fn request_id(&self) -> Option<&str> {
        self.request_id.as_deref()
    }

    /// The delay the endpoint asked for.
    #[must_use]
    pub const fn retry_after(&self) -> Option<Duration> {
        self.retry_after
    }

    /// The endpoint's own verdict on retrying, when it stated one.
    #[must_use]
    pub const fn should_retry(&self) -> Option<bool> {
        self.should_retry
    }

    /// Whether replaying the request can duplicate output a consumer already saw.
    #[must_use]
    pub const fn replay_safety(&self) -> ReplaySafety {
        self.replay_safety
    }

    /// Whether the call timed out. Derived from [`Self::kind`], never stored separately.
    #[must_use]
    pub const fn is_timeout(&self) -> bool {
        matches!(self.kind, ProviderErrorKind::Timeout)
    }

    /// Whether the call failed below the HTTP response: no answer ever arrived.
    ///
    /// A timeout is deliberately not folded in here. It is the one failure where the request may
    /// well have been received and acted on, so the two questions — "did the endpoint hear us" and
    /// "did we run out of patience" — have different answers and different consequences.
    #[must_use]
    pub const fn is_network_error(&self) -> bool {
        matches!(self.kind, ProviderErrorKind::Network)
    }

    /// Turns these facts into the error the caller receives, keeping them in its source chain.
    #[must_use]
    pub fn into_error(self) -> Error {
        let kind = self.kind;
        let message = self.message.clone();
        Error::provider(kind, message).with_source(self)
    }

    /// Recovers the facts from an error an adapter built with [`Self::into_error`].
    ///
    /// The whole source chain is walked rather than only its first link: an error may pick up
    /// wrapping on its way up, and facts that only survive an unwrapped path are facts a policy
    /// cannot rely on.
    #[must_use]
    pub fn from_error(error: &Error) -> Option<&Self> {
        let mut source = StdError::source(error);
        while let Some(current) = source {
            if let Some(normalized) = current.downcast_ref::<Self>() {
                return Some(normalized);
            }
            source = current.source();
        }
        None
    }
}

impl fmt::Display for NormalizedProviderError {
    /// Prints the facts, not the message.
    ///
    /// The message is already on the error this record is the source of, and repeating it there
    /// would make every log line say the same sentence twice.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut separator = "";
        if let Some(status) = self.status_code {
            write!(formatter, "HTTP {status}")?;
            separator = "; ";
        }
        if let Some(code) = &self.error_code {
            write!(formatter, "{separator}code {code}")?;
            separator = "; ";
        }
        if let Some(request_id) = &self.request_id {
            write!(formatter, "{separator}request_id {request_id}")?;
            separator = "; ";
        }
        if let Some(retry_after) = self.retry_after {
            write!(
                formatter,
                "{separator}retry after {}ms",
                retry_after.as_millis()
            )?;
            separator = "; ";
        }
        write!(formatter, "{separator}replay {:?}", self.replay_safety)
    }
}

impl StdError for NormalizedProviderError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        self.source
            .as_ref()
            .map(|source| source.as_ref() as &(dyn StdError + 'static))
    }
}

/// Restates a provider error with the replay verdict only the emitting layer can reach.
///
/// Replay safety is not a property of the failure; it is a property of what the consumer has
/// already been handed when the failure arrived. The byte reader that produced a transport error
/// cannot know that, and the decoder above it cannot rebuild an error without losing the source
/// chain — so the verdict is stamped on afterwards, in the one place that holds both facts.
///
/// Errors from other subsystems pass through untouched: a caller error raised while lowering a
/// request has no provider facts to restate, and inventing some would let a policy treat a code
/// defect as a network blip.
#[must_use]
pub fn stamp_replay_safety(error: Error, replay_safety: ReplaySafety) -> Error {
    match error {
        Error::Provider {
            kind,
            message,
            source,
        } => restate(kind, message, source, replay_safety),
        other => other,
    }
}

/// Rebuilds one provider error around an updated replay verdict.
fn restate(
    kind: ProviderErrorKind,
    message: String,
    source: Option<BoxError>,
    replay_safety: ReplaySafety,
) -> Error {
    let normalized = match source {
        // Already normalized: only the verdict changes, so every other fact the adapter recorded
        // survives the restatement.
        Some(source) => match source.downcast::<NormalizedProviderError>() {
            Ok(mut normalized) => {
                normalized.replay_safety = replay_safety;
                *normalized
            }
            Err(source) => NormalizedProviderError::new(kind, message)
                .with_replay_safety(replay_safety)
                .with_source(source),
        },
        None => NormalizedProviderError::new(kind, message).with_replay_safety(replay_safety),
    };
    normalized.into_error()
}

/// The replay verdict an error carries, or [`ReplaySafety::Unknown`] when it carries none.
///
/// The absent case reads as unknown rather than safe on purpose: an error that travelled through a
/// layer which never considered replay has said nothing about it, and a policy that reads silence
/// as approval will replay exactly the streams that must not be replayed.
#[must_use]
pub fn replay_safety_of(error: &Error) -> ReplaySafety {
    NormalizedProviderError::from_error(error).map_or(
        ReplaySafety::Unknown,
        NormalizedProviderError::replay_safety,
    )
}
