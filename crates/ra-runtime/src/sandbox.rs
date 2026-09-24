//! Sandbox execution: giving a sandbox agent the workspace it runs in.
//!
//! Ported from the reference's `run_config.py::SandboxRunConfig` and its `sandbox/runtime*.py`.
//! A sandbox agent is an ordinary [`AgentSpec`](ra_core::agent::AgentSpec) carrying a
//! [`SandboxAgentConfig`](ra_core::sandbox::SandboxAgentConfig); a run reaches the sandbox through
//! the [`SandboxRunConfig`] on its run configuration. Before each of a sandbox agent's turns the
//! runner prepares it — creating, resuming or borrowing its session, binding its capabilities to
//! that session, and assembling the prompt that describes the workspace — and when the run ends it
//! cleans up the sessions it owns and records what resumes them in the run's checkpoint.
//!
//! **Boundary**: this module names sandbox clients and sessions only through the protocol in
//! `ra-core`. No backend lives here; a host constructs one from a service crate and hands it in.
//!
//! # What this module does not do yet
//!
//! - A capability's context processing is not run for a sandbox agent's turns, and capabilities are
//!   not told the resolved model name when they adjust sampling settings.
//! - The reference's memory hooks, the instrumentation wrapper around sessions and the sanitization
//!   of the checkpoint's sandbox envelope as a whole are not here; each session state in it is
//!   sanitized by the client that wrote it.
//! - The runner's cleanup error is logged rather than surfaced, as on the reference, and a
//!   streamed run's live session is not exposed while the stream is running.

mod config;
mod preparation;
mod runtime;
mod session_manager;

use ra_core::{
    error::{Error, SandboxErrorKind},
    sandbox::SandboxError,
};

pub use config::SandboxRunConfig;
pub use preparation::{DEFAULT_SANDBOX_INSTRUCTIONS, filesystem_instructions};
pub(crate) use runtime::SandboxRuntime;

/// Carries a sandbox failure into the framework's error, keeping the original as its source.
///
/// Every sandbox failure lands under [`SandboxErrorKind::Setup`]: at this boundary it is the
/// sandbox a run needs that could not be prepared or released. A caller that branches on the
/// specific failure reads the [`SandboxError`] from the source and matches its code.
pub(crate) fn sandbox_error(error: SandboxError) -> Error {
    Error::sandbox(SandboxErrorKind::Setup, error.to_string()).with_source(error)
}
