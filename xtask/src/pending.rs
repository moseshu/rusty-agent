//! The three gates whose subject does not exist yet.
//!
//! They are not "to be implemented" placeholders — **there is nothing for the check to apply to**:
//! with no tool schema there are no bytes to compare, and with no guard registry there is no table
//! to reconcile. An empty function that pretends to pass would be worse than none, so each
//! honestly returns [`Outcome::Skip`] with the task that blocks it, and every `cargo xtask all`
//! lists them.
//!
//! Each one states its **enabling condition**: when that task lands, the implementation moves into
//! its own module and one entry disappears from here.

use crate::gate::Outcome;

/// Byte-level stability of tool schemas.
///
/// Enabling condition: R2-9 produces a renderable `ToolSchema`. The check will then be that the
/// same configuration renders byte-identically 100 times in a row (measured: Codex's 16 tool
/// schemas total 19,786 B and did not differ by a byte across 82 requests).
pub(crate) fn schema_stability() -> Outcome {
    Outcome::skip("R2-9", "尚无可渲染的工具 schema")
}

/// `Guard_Registry.md` agrees with the code, and hard-blocking guards number <= 8.
///
/// Enabling condition: R7-0 establishes the guard registry. The ceiling of 8 comes from
/// measurement — `AgentForge` piled up 44 guards and still lost to Codex's zero hard gates. A guard
/// is a safety net, not a tool for shaping behavior.
pub(crate) fn guard_registry() -> Outcome {
    Outcome::skip("R7-0", "尚无 guard 登记表")
}

/// Tool schemas <= 20 KB, and each prompt section within its token ceiling.
///
/// Enabling condition: both R2-9 and R4 have landed (both are subjects of the check). The measure
/// is what a **single request advertises**, not the total number of reachable tools.
pub(crate) fn token_budget() -> Outcome {
    Outcome::skip("R2-9 / R4", "尚无 schema 与 prompt 可计量")
}
