//! `RunState`: the serializable state of a whole run, with a schema version.
//!
//! R6-6 owns `RunState` itself. What already lives here is the run-scoped state R3 produces, and it
//! lives here rather than beside its producer for one reason: **persistence pins a wire format**.
//! The turn-settlement intermediates in [`step`](crate::step) are graded `Internal` and may be
//! refactored at any time; a value a saved checkpoint has to remain readable with cannot be, so it
//! takes this module's `Evolving` grade instead.

pub mod tool_use;

pub use tool_use::{
    AgentToolUse, ArgumentFingerprint, TOOL_USE_RECENT_LIMIT, TOOL_USE_SCHEMA_VERSION, ToolUse,
    ToolUseAttempt, ToolUseEntry, ToolUseRecord, ToolUseTracker,
};
