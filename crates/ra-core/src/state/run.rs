//! The run's own resumable state (the skeleton a future migration grows into the full checkpoint).
//!
//! A run has facts that are neither agent configuration nor session history: tool-use accounting,
//! future budget counters, and state owned by the loop itself. Keeping those values as independent
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
    compat::{SchemaVersion, Unknown},
    state::ToolUseTracker,
};

/// Current [`RunState`] schema version.
pub const RUN_STATE_SCHEMA_VERSION: SchemaVersion = SchemaVersion::new(1);

/// All mutable, framework-owned facts one run carries between its segments.
///
/// This is not host application context. Host context is an arbitrary live object passed to tools
/// through [`ToolRuntimeContext`](crate::tool::ToolRuntimeContext), while this value is safe to
/// checkpoint and restore. New framework-owned state is added here with a serde default; callers
/// pass the complete value through the runner's request API so a resumed run cannot accidentally
/// reset part of its accounting.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunState {
    #[serde(default = "run_state_schema_version")]
    schema_version: SchemaVersion,
    #[serde(default)]
    tool_use: ToolUseTracker,
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

    /// Mutable tool-use history for the loop's settlement path.
    #[doc(hidden)]
    pub fn tool_use_mut(&mut self) -> &mut ToolUseTracker {
        &mut self.tool_use
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
