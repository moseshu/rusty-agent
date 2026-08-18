//! Prefix stability invariants and invalidation tracking.

use std::fmt;

use ra_core::error::{Error, Result};
use ra_core::prompt::{ContentHash, PromptRole};

/// Explicit reasons that justify an intentional change to the stable prefix hash.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InvalidationReason {
    /// The agent specification was updated.
    AgentSpecChanged,
    /// The agent role was switched.
    RoleChanged(PromptRole),
    /// An explicit manual reload or hot reconfiguration occurred.
    ManualReload(String),
}

impl fmt::Display for InvalidationReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AgentSpecChanged => formatter.write_str("agent specification changed"),
            Self::RoleChanged(role) => write!(formatter, "role changed to `{role}`"),
            Self::ManualReload(reason) => write!(formatter, "manual reload: {reason}"),
        }
    }
}

/// Tracks the stable prefix hash across interaction turns and asserts cache invariance.
#[derive(Clone, Debug, Default)]
pub struct PrefixStabilityTracker {
    last_prefix_hash: Option<ContentHash>,
    invalidation_count: usize,
}

impl PrefixStabilityTracker {
    /// Creates a new prefix stability tracker.
    #[must_use]
    pub fn new() -> Self {
        Self {
            last_prefix_hash: None,
            invalidation_count: 0,
        }
    }

    /// Records the prefix hash for a new turn, asserting that it matches the previous turn's hash
    /// unless an explicit invalidation reason is supplied.
    ///
    /// # Errors
    ///
    /// Returns an error if the prefix hash changed unexpectedly without an invalidation reason.
    pub fn record_turn(
        &mut self,
        current_hash: &ContentHash,
        invalidation_reason: Option<InvalidationReason>,
    ) -> Result<()> {
        if let Some(prev) = &self.last_prefix_hash
            && prev != current_hash
        {
            if let Some(reason) = invalidation_reason {
                tracing::info!(
                    previous_hash = %prev,
                    new_hash = %current_hash,
                    %reason,
                    "prompt prefix invalidated intentionally"
                );
                self.invalidation_count += 1;
            } else {
                return Err(Error::caller(format!(
                    "stable prefix hash changed unexpectedly from `{prev}` to \
                     `{current_hash}`; volatile modifications must use tail messages rather \
                     than mutating the stable prefix"
                )));
            }
        }
        self.last_prefix_hash = Some(current_hash.clone());
        Ok(())
    }

    /// Asserts that the given hash matches the last recorded prefix hash.
    ///
    /// # Errors
    ///
    /// Returns an error if the hash differs from the tracked prefix.
    pub fn assert_prefix_invariance(&self, current_hash: &ContentHash) -> Result<()> {
        if let Some(prev) = &self.last_prefix_hash
            && prev != current_hash
        {
            return Err(Error::caller(format!(
                "prefix hash invariance assertion failed: expected `{prev}`, got \
                 `{current_hash}`"
            )));
        }
        Ok(())
    }

    /// Returns the last recorded prefix hash, if any.
    #[must_use]
    pub const fn last_prefix_hash(&self) -> Option<&ContentHash> {
        self.last_prefix_hash.as_ref()
    }

    /// Returns the number of times the prefix was intentionally invalidated.
    #[must_use]
    pub const fn invalidation_count(&self) -> usize {
        self.invalidation_count
    }
}

/// Asserts that two prefix hashes are identical, or that a valid invalidation reason is present.
///
/// # Errors
///
/// Returns an error if hashes differ without an invalidation reason.
pub fn assert_prefix_stable(
    previous_hash: &ContentHash,
    current_hash: &ContentHash,
    reason: Option<&InvalidationReason>,
) -> Result<()> {
    if previous_hash != current_hash && reason.is_none() {
        return Err(Error::caller(format!(
            "stable prefix hash changed unexpectedly from `{previous_hash}` to `{current_hash}`"
        )));
    }
    Ok(())
}
