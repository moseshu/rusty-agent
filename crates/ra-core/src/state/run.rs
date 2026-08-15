//! The run's own resumable state (the skeleton a future migration grows into the full checkpoint).
//!
//! A run has facts that are neither agent configuration nor session history: tool-use accounting,
//! budget counters, and state owned by the loop itself. Keeping those values as independent
//! fields on the runner would make a new fact a signature change across every entry point and would
//! let a continuation accidentally carry one fact but not another. `RunState` is the single carrier
//! that crosses a run-segment boundary.
//!
//! It deliberately belongs to `ra-core`: a future migration grows this same value into the full
//! serializable run state (generated items, model responses, pending approvals, guardrail
//! results), and a persisted wire type cannot live in `ra-runtime` without reversing the
//! dependency direction.
//!
//! # Not to be confused with `WorkState`
//!
//! This is a **single run's** recoverable state. A future cross-run `WorkState`, reached through
//! [`WorkStateHandle`](crate::state::work::WorkStateHandle), is the **cross-run, cross-node task**
//! state — owned by whoever spans those runs, not by this value. Mixing them is forbidden, and the
//! reason is concrete: a checkpoint of this value is scoped to one run's resume, while task state
//! outlives every run that touches it. Folding the second into the first would make a resumed run
//! restore a stale copy of state another node has since advanced.

use serde::{Deserialize, Serialize};

use crate::{
    budget::BudgetSnapshot,
    compat::{SchemaVersion, Unknown},
    state::{ToolFailureTracker, ToolUseTracker},
};

/// Current [`RunState`] schema version.
pub const RUN_STATE_SCHEMA_VERSION: SchemaVersion = SchemaVersion::new(1);

/// Stable identity of one run, across every segment it is resumed in.
///
/// It lives beside the recoverable state rather than beside the live
/// [`RunContext`](crate::context::RunContext) on purpose. Persisted events, stored items and replay
/// all attribute by this ID, so the run's identity has to be a value that survives a checkpoint;
/// the live context only ever shows a read view of it.
///
/// # Where an ID comes from
///
/// From whoever starts the run, as an explicit construction argument. [`Self::generate`] mints one,
/// but it has to be *called* — there is deliberately no `Default` and no minting inside another
/// constructor, because an ID a value type produces on its own reappears in two places that must
/// not have it: a `Default` that quietly acquires an identity, and a deserialization that hands a
/// restored run a brand new one and detaches it from everything already written under the old.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunId(String);

impl RunId {
    /// Creates an ID from a host-generated string.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Mints a fresh random ID for a run that is starting now.
    #[must_use]
    pub fn generate() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    /// String representation.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl core::fmt::Display for RunId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// All mutable, framework-owned facts one run carries between its segments.
///
/// This is not host application context. Host context is an arbitrary live object read through
/// [`RunContext::app_context`](crate::context::RunContext::app_context), while this value is safe
/// to checkpoint and restore. New framework-owned state is added here with a serde default; callers
/// pass the complete value through the runner's request API so a resumed run cannot accidentally
/// reset part of its accounting.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunState {
    #[serde(default = "run_state_schema_version")]
    schema_version: SchemaVersion,
    #[serde(default)]
    tool_use: ToolUseTracker,
    #[serde(default)]
    tool_failure: ToolFailureTracker,
    #[serde(default)]
    budget: BudgetSnapshot,
    #[serde(flatten, default, skip_serializing_if = "Unknown::is_empty")]
    unknown: Unknown,
}

impl Default for RunState {
    fn default() -> Self {
        Self::new()
    }
}

impl RunState {
    /// Creates empty state for a new run.
    #[must_use]
    pub fn new() -> Self {
        Self {
            schema_version: RUN_STATE_SCHEMA_VERSION,
            tool_use: ToolUseTracker::new(),
            tool_failure: ToolFailureTracker::new(),
            budget: BudgetSnapshot::new(),
            unknown: Unknown::new(),
        }
    }

    /// Replaces the carried tool-use history, leaving every other field alone.
    ///
    /// This is the seam for code that persisted the `ToolUseTracker` on its own before `RunState`
    /// existed: build the state, then set the one field it has.
    #[must_use]
    pub fn with_tool_use(mut self, tool_use: ToolUseTracker) -> Self {
        self.tool_use = tool_use;
        self
    }

    /// Schema version of this value.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Tool-use history as of the most recently settled turn.
    #[must_use]
    pub const fn tool_use(&self) -> &ToolUseTracker {
        &self.tool_use
    }

    /// Replaces the carried failure history, leaving every other field alone.
    #[must_use]
    pub fn with_tool_failure(mut self, tool_failure: ToolFailureTracker) -> Self {
        self.tool_failure = tool_failure;
        self
    }

    /// Mutable tool-use history for the loop's settlement path.
    #[doc(hidden)]
    pub fn tool_use_mut(&mut self) -> &mut ToolUseTracker {
        &mut self.tool_use
    }

    /// Failure history as of the most recently settled turn.
    #[must_use]
    pub const fn tool_failure(&self) -> &ToolFailureTracker {
        &self.tool_failure
    }

    /// Recoverable budget accounting as of the most recently completed operation.
    #[must_use]
    pub const fn budget(&self) -> &BudgetSnapshot {
        &self.budget
    }

    /// Replaces budget accounting while preserving all other run state.
    #[must_use]
    pub fn with_budget(mut self, budget: BudgetSnapshot) -> Self {
        self.budget = budget;
        self
    }

    /// Mutable budget accounting for the runner.
    #[doc(hidden)]
    pub fn budget_mut(&mut self) -> &mut BudgetSnapshot {
        &mut self.budget
    }

    /// Both trackers at once, for the settlement path that records into each.
    ///
    /// One method rather than two: settlement holds the call trail and the failure history for the
    /// same turn, and two separate `&mut` accessors would each borrow the whole state, so a caller
    /// could not hold both. Splitting the borrow here keeps that from pushing `RunState` itself —
    /// the checkpoint type — through a settlement signature.
    #[doc(hidden)]
    pub fn trackers_mut(&mut self) -> (&mut ToolUseTracker, &mut ToolFailureTracker) {
        (&mut self.tool_use, &mut self.tool_failure)
    }

    /// Unknown fields retained during deserialization.
    #[must_use]
    pub const fn unknown(&self) -> &Unknown {
        &self.unknown
    }
}

const fn run_state_schema_version() -> SchemaVersion {
    RUN_STATE_SCHEMA_VERSION
}
