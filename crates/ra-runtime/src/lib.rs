//! # `ra-runtime`
//!
//! The loop kernel. Its public API must be able to run any agent and contains no business vocabulary.
//!
//! **Boundary**: it depends only on the protocol-neutral contracts in `ra-core`. The public surface
//! is the agent definition, the runner entry, the tool profile / registry, and the guard registry;
//! turn settlement, dispatch, budget enforcement, the loop breaker, and the approval flow are all
//! crate-internal and cannot be bypassed downstream.
//!
//! **Stability**: the `Runner` entry signature is `Stable` (it is on the Stable API list); the
//! turn-settlement intermediates are `Internal` — `ProcessedResponse` / `ToolExecutionPlan` /
//! `SingleStepResult` may be refactored at any time, so do not depend on them from outside.

pub mod agent;
pub(crate) mod budget;
pub(crate) mod capability;
pub(crate) mod circuit;
pub mod guard;
pub(crate) mod hook;
pub(crate) mod permission;
pub mod runner;
pub mod tool;
pub(crate) mod turn;
