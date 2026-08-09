//! # `ra-runtime`
//!
//! The loop kernel. Its public API must be able to run any agent and contains no business vocabulary.
//!
//! **Boundary**: it depends only on the protocol-neutral contracts in `ra-core`. The supported
//! surface is the agent definition, the runner entry, the tool profile / registry, and the guard
//! registry; turn settlement, dispatch, budget enforcement, the loop breaker, and the approval flow
//! are not part of it.
//!
//! **Stability**: the `Runner` entry signature is `Stable` (it is on the Stable API list); the
//! turn-settlement machinery is `Internal` — the stage functions here and the `ProcessedResponse` /
//! `ToolExecutionPlan` / `SingleStepResult` types they produce in `ra-core::step` may be refactored
//! at any time, so do not depend on them from outside.
//!
//! [`turn`] is `Internal` but technically reachable, and that is a deliberate trade rather than an
//! oversight: tests live in a separate workspace (no test code under `crates/`), so a module with
//! no `pub` path has no way to be tested at all. It carries `#[doc(hidden)]`, appears in the
//! public-API baseline so its churn stays visible in review, and has **no compatibility promise** —
//! `#[doc(hidden)]` documents intent, it does not enforce it.

pub mod agent;
pub(crate) mod budget;
pub(crate) mod capability;
pub(crate) mod circuit;
pub mod guard;
pub(crate) mod hook;
pub(crate) mod permission;
pub mod runner;
pub mod tool;
#[doc(hidden)]
pub mod turn;
