//! Gate results.
//!
//! Three states rather than two: **SKIP is not PASS**.
//!
//! One of the nine gates still has nothing to check (the guard registry waits on R7-0). Such an
//! entry could either print a "not implemented" placeholder or pretend to pass; the first reads as
//! done, and the second is worse because it manufactures confidence.
//! So [`Outcome::Skip`] is its own tier, carries **what unblocks it**, and is counted separately in
//! the summary: CI never turns red for it, but every run shows how many are still owed.
//!
//! There are two ways to be blocked, and `blocked_by` names either. One is pending work, as with
//! the guard registry. The other is a precondition this repository declines to ship: the three
//! reconciling gates read baselines under `api/`, which is deliberately not in version control, so
//! a fresh checkout has nothing to compare against and the field names the bless command that would
//! produce it. Both are the same statement — this run did not check the thing — and neither may
//! read as a pass.

use core::fmt;

/// The result of running one gate.
pub(crate) enum Outcome {
    /// Passed, with one line describing what was checked.
    Pass(String),
    /// Preconditions were absent, so it did not run.
    Skip {
        /// Why it was skipped.
        reason: String,
        /// The task blocking it, such as `R2-9`.
        blocked_by: &'static str,
    },
    /// Failed, listing each violation.
    Fail(Vec<String>),
}

impl Outcome {
    /// Passes.
    pub(crate) fn pass(detail: impl Into<String>) -> Self {
        Self::Pass(detail.into())
    }

    /// Skips.
    pub(crate) fn skip(blocked_by: &'static str, reason: impl Into<String>) -> Self {
        Self::Skip {
            reason: reason.into(),
            blocked_by,
        }
    }

    /// Passes when there are no violations, fails otherwise.
    pub(crate) fn from_violations(violations: Vec<String>, detail: impl Into<String>) -> Self {
        if violations.is_empty() {
            Self::pass(detail)
        } else {
            Self::Fail(violations)
        }
    }

    /// Whether this counts as a failure. **Only this turns CI red.**
    pub(crate) const fn is_failure(&self) -> bool {
        matches!(self, Self::Fail(_))
    }

    /// Whether it was skipped.
    pub(crate) const fn is_skipped(&self) -> bool {
        matches!(self, Self::Skip { .. })
    }

    /// Fixed-width status label, so the summary table lines up.
    pub(crate) const fn status(&self) -> &'static str {
        match self {
            Self::Pass(_) => "PASS",
            Self::Skip { .. } => "SKIP",
            Self::Fail(_) => "FAIL",
        }
    }
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pass(detail) => f.write_str(detail),
            Self::Skip { reason, blocked_by } => write!(f, "{reason}（等 {blocked_by}）"),
            Self::Fail(violations) => write!(f, "{} 处违规", violations.len()),
        }
    }
}
