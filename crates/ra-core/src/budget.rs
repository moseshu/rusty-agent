//! Protocol-neutral limits and accumulated budget usage.
//!
//! A limit describes what one run segment is allowed to spend. A snapshot is the recoverable
//! accounting state that crosses a continuation boundary. Keeping these separate means a caller
//! can resume the same work with a larger allowance without resetting what was already spent.
//!
//! The split is also what keeps the snapshot honest across a checkpoint. Every ceiling — including
//! the wall clock, whose monotonic instant cannot be restored in another process — lives on the
//! limit, which is configuration and is never persisted. The snapshot therefore holds spent
//! counters and nothing else: it serializes completely, and two snapshots that compare equal really
//! do describe the same spend.
//!
//! # The token dimension is not counted here
//!
//! Turns are the one thing this snapshot counts, because a turn is the loop's own event and nothing
//! else records it. Tokens are reported by the provider and already accumulate in the run's usage
//! ledger, so a `tokens_used` field here would be a second counter incremented from the same
//! settlement point — and two counters that must agree eventually stop agreeing, in the one place
//! where the disagreement means a run either overspends or stops early. The token ceiling is
//! therefore evaluated where both facts are held together, by
//! [`RunState`](crate::state::RunState), against the ledger it owns.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::{
    cancel::Deadline,
    compat::{SchemaVersion, Unknown},
};

/// Current [`BudgetSnapshot`] schema version.
pub const BUDGET_SNAPSHOT_SCHEMA_VERSION: SchemaVersion = SchemaVersion::new(1);

/// Limits applied to a run.
///
/// Amounts use integer units.
///
/// # Why there is no spend ceiling
///
/// Money is deliberately not a dimension here. No provider reports a charge alongside its usage,
/// so a framework-side amount could only come from a built-in price table — one that is per
/// provider, per model, per token class, changes without notice, and would be wrong silently. The
/// authoritative per-request token counts are preserved on every `ModelResponse` and in the run's
/// usage ledger, which is what a host needs to apply its own contracted rates. Tokens are the unit
/// both sides can agree on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BudgetLimit {
    max_turns: Option<u32>,
    max_tokens: Option<u64>,
    deadline: Option<Deadline>,
}

impl BudgetLimit {
    /// Creates an unlimited budget.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            max_turns: None,
            max_tokens: None,
            deadline: None,
        }
    }

    /// Sets the maximum number of model turns.
    #[must_use]
    pub const fn with_max_turns(mut self, max_turns: u32) -> Self {
        self.max_turns = Some(max_turns);
        self
    }

    /// Sets the maximum total token usage reported by model responses.
    #[must_use]
    pub const fn with_max_tokens(mut self, max_tokens: u64) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }

    /// Sets the absolute wall-clock deadline.
    #[must_use]
    pub const fn with_deadline(mut self, deadline: Deadline) -> Self {
        self.deadline = Some(deadline);
        self
    }

    /// Maximum model turns, if one was set.
    #[must_use]
    pub const fn max_turns(&self) -> Option<u32> {
        self.max_turns
    }

    /// Maximum total tokens, if one was set.
    #[must_use]
    pub const fn max_tokens(&self) -> Option<u64> {
        self.max_tokens
    }

    /// Absolute deadline, if one was set.
    #[must_use]
    pub const fn deadline(&self) -> Option<Deadline> {
        self.deadline
    }

    /// Remaining wall-clock time, if a deadline was set. Zero once it has expired.
    #[must_use]
    pub fn remaining_wall_clock(&self) -> Option<Duration> {
        self.deadline.map(Deadline::remaining)
    }

    /// Rejects zero-valued ceilings, which would otherwise look like an unlimited budget in many
    /// caller-side calculations while allowing no useful work.
    pub fn validate(&self) -> crate::error::Result<()> {
        for (name, value) in [
            ("max_turns", self.max_turns.map(u64::from)),
            ("max_tokens", self.max_tokens),
        ] {
            if value == Some(0) {
                return Err(crate::error::Error::config(format!(
                    "`{name}` must be at least 1 when configured"
                )));
            }
        }
        Ok(())
    }
}

/// Recoverable turn accounting for a run.
///
/// Every counter is spend, so the whole value survives a checkpoint. What was spent is the only
/// thing a continuation has to carry: the ceilings it will be measured against arrive with the new
/// [`BudgetLimit`], which is how a caller resumes the same work with a larger allowance. Nothing
/// process-local lives here — the wall clock is on the limit precisely because a monotonic instant
/// cannot be restored — so a clean record round-trips to an equal value.
///
/// Token spend is not among these counters; see the [module docs](self) for why it is read from the
/// run's usage ledger instead of being counted a second time here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetSnapshot {
    #[serde(default = "budget_snapshot_schema_version")]
    schema_version: SchemaVersion,
    #[serde(default)]
    turns_used: u32,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl Default for BudgetSnapshot {
    fn default() -> Self {
        Self::new()
    }
}

impl BudgetSnapshot {
    /// Creates an empty snapshot.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            schema_version: BUDGET_SNAPSHOT_SCHEMA_VERSION,
            turns_used: 0,
            unknown: Unknown::new(),
        }
    }

    /// Schema version of this checkpoint record.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Records one started model turn.
    pub fn record_turn(&mut self) {
        self.turns_used = self.turns_used.saturating_add(1);
    }

    /// Number of model turns that have started.
    #[must_use]
    pub const fn turns_used(&self) -> u32 {
        self.turns_used
    }

    /// Fields written by a newer version and retained across a downgrade read/write cycle.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }

    /// Remaining turn allowance, if turns are limited.
    #[must_use]
    pub fn remaining_turns(&self, limit: &BudgetLimit) -> Option<u32> {
        limit
            .max_turns
            .map(|maximum| maximum.saturating_sub(self.turns_used))
    }

    /// Whether the turn allowance is used up.
    ///
    /// One dimension rather than a verdict on the whole budget: answering that means holding the
    /// usage ledger too, which is why [`RunState`](crate::state::RunState) is the one that answers
    /// it.
    #[must_use]
    pub fn turns_exhausted(&self, limit: &BudgetLimit) -> bool {
        limit
            .max_turns
            .is_some_and(|maximum| self.turns_used >= maximum)
    }
}

const fn budget_snapshot_schema_version() -> SchemaVersion {
    BUDGET_SNAPSHOT_SCHEMA_VERSION
}
