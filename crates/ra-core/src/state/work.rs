//! The mount point for cross-run task state.
//!
//! `WorkState` itself is future work: typed channels with reducers, each carrying a version and
//! the node, run, and turn that last wrote it. **None of that is here, deliberately.** What is
//! here is the slot it will arrive through — a handle the run carries and hands to every tool, and
//! later to every guard.
//!
//! # Why a slot this early
//!
//! The cost is asymmetric. Reserving the slot today is one accessor on
//! [`ToolServices`](crate::tool::ToolServices), the bag every call is handed. Adding it later
//! instead means touching every construction point of the run context *and every caller of*
//! [`Tool::call`](crate::tool::Tool::call) — including third-party tools, whose signatures the
//! framework does not own. A milestone that cannot be added without a breaking change to other
//! people's code has to reserve its seam before it is needed.
//!
//! # Why a handle and not a value
//!
//! Task state spans runs and, eventually, nodes: two runs of the same task look at one state, and
//! whoever advances a channel does it for everyone. A value copied into
//! [`RunState`](crate::state::run::RunState) would make each run's checkpoint carry a private
//! snapshot that goes stale the moment another node writes — which is exactly the mixing this
//! module forbids. The handle keeps ownership outside the run.

use core::any::Any;

/// Access to the task state a run participates in.
///
/// Implement it on whatever already owns the task's state; a future milestone supplies the
/// framework's own implementation together with the channel and reducer model. Until then the
/// trait carries **no channel operations at all**, because a reducer-free `get` / `set` pair
/// would be the wrong shape to grow into one — the whole point of the eventual channel model is
/// that concurrent writers merge by a declared rule rather than by last-write-wins.
///
/// What a host can do with it today is downcast to its own type, which is enough for a tool to
/// read task state a host is already keeping, and costs nothing to keep working once a future
/// milestone adds the typed operations above it.
pub trait WorkStateHandle: Any + Send + Sync {
    /// Enables checked downcasting to the concrete task-state type.
    fn as_any(&self) -> &(dyn Any + Send + Sync);
}
