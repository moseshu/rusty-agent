//! Structured diagnostics and error-scoped retry metadata.

use std::{
    error::Error as StdError,
    fmt,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use ra_core::{
    error::{BoxError, Error},
    model::RetryAdvice,
};

use super::ModelCall;

#[derive(Debug, thiserror::Error)]
#[error(
    "Automatic streaming does not support {item_kind}. Use ModelStep::stream for exact events."
)]
pub(super) struct UnsupportedAutomaticStream {
    item_kind: &'static str,
}

impl UnsupportedAutomaticStream {
    pub(super) const fn new(item_kind: &'static str) -> Self {
        Self { item_kind }
    }
}

/// A model call arrived after all configured steps had been consumed.
///
/// The message numbers calls from one, as upstream does; [`Self::call_index`] stays zero-based.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
#[error(
    "Unexpected {} model call #{}: no scripted steps remain.",
    call_mode(.call.streamed),
    .call_index + 1
)]
pub struct UnexpectedModelCall {
    call: Box<ModelCall>,
    call_index: usize,
}

impl UnexpectedModelCall {
    pub(super) fn new(call: ModelCall, call_index: usize) -> Self {
        Self {
            call: Box::new(call),
            call_index,
        }
    }

    /// Detached snapshot of the unexpected request.
    #[must_use]
    pub fn call(&self) -> &ModelCall {
        &self.call
    }

    /// Zero-based call index, including previous unexpected calls.
    #[must_use]
    pub const fn call_index(&self) -> usize {
        self.call_index
    }
}

const fn call_mode(streamed: bool) -> &'static str {
    if streamed {
        "streaming"
    } else {
        "non-streaming"
    }
}

/// Configured calls remain when checking completion.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
#[error("{remaining_steps} scripted model step(s) were not consumed.")]
pub struct UnconsumedModelSteps {
    remaining_steps: usize,
}

impl UnconsumedModelSteps {
    pub(super) const fn new(remaining_steps: usize) -> Self {
        Self { remaining_steps }
    }

    /// Number of unconsumed calls.
    #[must_use]
    pub const fn remaining_steps(&self) -> usize {
        self.remaining_steps
    }
}

/// Retry advice carried in an error's source chain.
///
/// Rust errors move by value, so the upstream lookup by exception identity has no counterpart.
/// Instead this node replaces the error's immediate source and keeps the original source beneath
/// it. Classification and the error's own display are unchanged, and lookups that walk the whole
/// chain, such as `NormalizedProviderError::from_error`, still reach the original facts. Code
/// that downcasts only the immediate source sees this node instead, a chain renderer prints the
/// original source's text twice, and a source-less error gains this node as its only source.
#[derive(Debug)]
struct ErrorAdvice {
    identity: Arc<()>,
    advice: RetryAdvice,
    active: AtomicBool,
    original: Option<BoxError>,
}

impl fmt::Display for ErrorAdvice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(original) = &self.original {
            original.fmt(f)
        } else {
            f.write_str("scripted model retry advice")
        }
    }
}

impl StdError for ErrorAdvice {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        self.original.as_deref().map(|source| source as _)
    }
}

pub(super) fn configure_retry_advice(
    error: &mut Error,
    identity: Arc<()>,
    advice: Option<RetryAdvice>,
) {
    let mut current: Option<&(dyn StdError + 'static)> = Some(error);
    while let Some(source) = current {
        if let Some(metadata) = source.downcast_ref::<ErrorAdvice>()
            && Arc::ptr_eq(&metadata.identity, &identity)
        {
            metadata.active.store(false, Ordering::Relaxed);
        }
        current = source.source();
    }
    if let Some(advice) = advice {
        attach_retry_advice(error, identity, advice);
    }
}

fn attach_retry_advice(error: &mut Error, identity: Arc<()>, advice: RetryAdvice) {
    let source = match error {
        Error::Run { error, .. } => return attach_retry_advice(error, identity, advice),
        Error::Config { source, .. }
        | Error::Caller { source, .. }
        | Error::InvalidFinalOutput { source, .. }
        | Error::Provider { source, .. }
        | Error::Tool { source, .. }
        | Error::Sandbox { source, .. }
        | Error::Session { source, .. }
        | Error::Protocol { source, .. } => source,
        _ => return,
    };
    *source = Some(Box::new(ErrorAdvice {
        identity,
        advice,
        active: AtomicBool::new(true),
        original: source.take(),
    }));
}

pub(super) fn retry_advice(error: &Error, identity: &Arc<()>) -> Option<RetryAdvice> {
    let mut current: Option<&(dyn StdError + 'static)> = Some(error);
    while let Some(error) = current {
        if let Some(metadata) = error.downcast_ref::<ErrorAdvice>()
            && Arc::ptr_eq(&metadata.identity, identity)
            && metadata.active.load(Ordering::Relaxed)
        {
            return Some(metadata.advice.clone());
        }
        current = error.source();
    }
    None
}
