//! # `ra-eval`
//!
//! Eval, replay, trace assertions, the regression flywheel.
//!
//! **Boundary**: it consumes the framework's public results and traces. It takes no part in the
//! control flow of a live run, injects no product special cases into the kernel, and is not a
//! production persistence backend.
//! Local run records and provider-neutral spans are its inputs. An external trace exporter is an
//! optional host integration, independent of the model provider and outside the default runtime.
//!
//! **Stability**: `Evolving`. Assertion and reporting APIs follow eval needs.

pub mod assert;
pub mod fixture;
pub mod replay;
pub mod report;
