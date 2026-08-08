//! # `ra-eval`
//!
//! Eval, replay, trace assertions, the regression flywheel.
//!
//! **Boundary**: it consumes the framework's public results and traces. It takes no part in the
//! control flow of a live run, injects no product special cases into the kernel, and is not a
//! production persistence backend.
//!
//! **Stability**: `Evolving`. Assertion and reporting APIs follow eval needs.

pub mod assert;
pub mod fixture;
pub mod replay;
pub mod report;
