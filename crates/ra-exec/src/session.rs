//! Process execution session identity and audited state transitions.

use ra_core::event::exec::ExecEvictionReason;
pub use ra_core::event::exec::ExecSessionId;
use serde::{Deserialize, Serialize};

/// The lifecycle state of a process execution session.
///
/// Transitions follow an audited single-direction state machine. Once a session reaches a terminal
/// state ([`Self::Exited`], [`Self::Failed`], [`Self::Cancelled`], [`Self::Expired`]), no further
/// transitions are permitted.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ExecSessionState {
    /// Session allocated and reserved, but the child process has not yet spawned.
    Reserved,
    /// Process spawning in progress.
    Starting,
    /// Process running and accepting output collection / interactions.
    Running,
    /// Process exited normally or by signal.
    Exited {
        /// Process exit status code, if available.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
    },
    /// Process spawn or communication failed.
    Failed {
        /// Failure explanation.
        error: String,
    },
    /// Execution was explicitly cancelled by the caller or deadline.
    Cancelled,
    /// Session was expired or evicted by host policy.
    Expired {
        /// Eviction reason.
        reason: ExecEvictionReason,
    },
}

/// An error returned when attempting an illegal state machine transition.
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
#[error("invalid execution session state transition from {from:?} to {to:?}")]
pub struct ExecSessionTransitionError {
    from: ExecSessionState,
    to: ExecSessionState,
}

impl ExecSessionTransitionError {
    /// Creates a new transition error.
    #[must_use]
    pub const fn new(from: ExecSessionState, to: ExecSessionState) -> Self {
        Self { from, to }
    }

    /// State before transition.
    #[must_use]
    pub const fn from(&self) -> &ExecSessionState {
        &self.from
    }

    /// Target state attempted.
    #[must_use]
    pub const fn to(&self) -> &ExecSessionState {
        &self.to
    }
}

impl ExecSessionState {
    /// Whether this state is a terminal final state.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Exited { .. } | Self::Failed { .. } | Self::Cancelled | Self::Expired { .. }
        )
    }

    /// Whether this session is actively running or starting.
    #[must_use]
    pub const fn is_active(&self) -> bool {
        matches!(self, Self::Reserved | Self::Starting | Self::Running)
    }

    /// Returns true if transitioning from `self` to `next` is valid.
    #[must_use]
    pub fn can_transition_to(&self, next: &Self) -> bool {
        match self {
            Self::Reserved => {
                matches!(next, Self::Starting | Self::Failed { .. } | Self::Cancelled)
            }
            Self::Starting => matches!(
                next,
                Self::Running | Self::Exited { .. } | Self::Failed { .. } | Self::Cancelled
            ),
            Self::Running => matches!(
                next,
                Self::Exited { .. } | Self::Failed { .. } | Self::Cancelled | Self::Expired { .. }
            ),
            Self::Exited { .. } | Self::Failed { .. } | Self::Cancelled | Self::Expired { .. } => {
                false
            }
        }
    }

    /// Attempts to transition the state machine to `next`.
    ///
    /// # Errors
    ///
    /// Returns [`ExecSessionTransitionError`] if the transition is not permitted.
    pub fn transition_to(&mut self, next: Self) -> Result<(), ExecSessionTransitionError> {
        if self.can_transition_to(&next) {
            *self = next;
            Ok(())
        } else {
            Err(ExecSessionTransitionError {
                from: self.clone(),
                to: next,
            })
        }
    }
}
