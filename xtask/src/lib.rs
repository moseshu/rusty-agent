//! Testable policy primitives used by the repository gates.

pub mod baseline_policy;
pub mod layering_policy;
pub mod parity_policy;
pub mod tests_workspace_policy;

/// Extension-safety checks and precisely scoped upstream record exceptions.
pub mod extension_safety_policy;
