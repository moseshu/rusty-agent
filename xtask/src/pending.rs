//! The gate whose subject does not exist yet.
//!
//! It is not a "to be implemented" placeholder — **there is nothing for the check to apply to**:
//! with no guard registry there is no table to reconcile. A function that pretended to pass would
//! be worse than none, so it honestly returns [`Outcome::Skip`] carrying the task that blocks it,
//! and every `cargo xtask all` lists it.
//!
//! It states that **enabling condition** below: when the task lands, the implementation moves into
//! its own module and this one goes away.

use crate::gate::Outcome;

/// `Guard_Registry.md` agrees with the code, and hard-blocking guards number <= 8.
///
/// Enabling condition: R7-0 establishes the guard registry. The ceiling of 8 comes from
/// measurement — `AgentForge` piled up 44 guards and still lost to Codex's zero hard gates. A guard
/// is a safety net, not a tool for shaping behavior.
pub(crate) fn guard_registry() -> Outcome {
    Outcome::skip("R7-0", "尚无 guard 登记表")
}
