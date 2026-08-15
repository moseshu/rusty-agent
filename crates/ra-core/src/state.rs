//! `RunState`: the serializable state of a whole run, with a schema version.
//!
//! The run-scoped state turn settlement produces lives here rather than beside its producer for
//! one reason: **persistence pins a wire format**. The turn-settlement intermediates in
//! [`step`](crate::step) are graded `Internal` and may be refactored at any time; a value a saved
//! checkpoint has to remain readable with cannot be, so it takes this module's `Evolving` grade
//! instead. A future migration grows [`RunState`] into the full checkpoint by adding fields to it.
//!
//! [`work`] is the other half of that separation and holds no state at all: the task that spans
//! runs is reached through a handle, so a run's checkpoint cannot come to contain a private copy of
//! it.

pub mod run;
pub mod tool_failure;
pub mod tool_use;
pub mod work;

pub use run::{RUN_STATE_SCHEMA_VERSION, RunId, RunState};
pub use tool_failure::{
    AgentToolFailures, EvidenceFingerprint, TOOL_FAILURE_RECENT_LIMIT, TOOL_FAILURE_SCHEMA_VERSION,
    ToolFailureEntry, ToolFailureRecord, ToolFailureTracker, ToolOutcome,
};
pub use tool_use::{
    AgentToolUse, ArgumentFingerprint, TOOL_USE_RECENT_LIMIT, TOOL_USE_SCHEMA_VERSION, ToolUse,
    ToolUseAttempt, ToolUseEntry, ToolUseRecord, ToolUseTracker,
};
pub use work::WorkStateHandle;
