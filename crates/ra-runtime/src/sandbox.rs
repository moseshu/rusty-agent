//! Sandbox execution: giving a sandbox agent the workspace it runs in.
//!
//! Ported from the reference's `run_config.py::SandboxRunConfig` and its `sandbox/runtime*.py`.
//! A sandbox agent is an ordinary [`AgentSpec`](ra_core::agent::AgentSpec) carrying a
//! [`SandboxAgentConfig`](ra_core::sandbox::SandboxAgentConfig); a run reaches the sandbox through
//! the [`SandboxRunConfig`] on its run configuration. Before each of a sandbox agent's turns the
//! runner prepares it — creating, resuming or borrowing its session, binding its capabilities to
//! that session, and assembling the prompt that describes the workspace — and when the run ends it
//! cleans up the sessions it owns and records what resumes them in the run's checkpoint. When the
//! agent it prepared last carries a generating memory capability, the run is also recorded for
//! [`memory`] generation before that cleanup.
//!
//! **Boundary**: this module names sandbox clients and sessions only through the protocol in
//! `ra-core`. No backend lives here; a host constructs one from a service crate and hands it in.
//!
//! # Where this differs from the reference
//!
//! - The instrumentation wrapper around sessions lives with the backends, in the service crate:
//!   the built-in clients hand back sessions already wrapped, as the reference's do, and a host's
//!   own client decides for itself. This module still replaces a failure that may quote mount
//!   authority where it calls into a session — preparing it, cleaning it up — so a session that
//!   arrives unwrapped is covered at those points too.
//! - The runner's cleanup error is logged rather than surfaced, as on the reference. The
//!   reference also parks the live session on a streamed result in a private attribute while the
//!   stream runs; nothing here carries it, since nothing public reads it.

mod config;
pub mod memory;
mod preparation;
mod runtime;
mod session_manager;

use ra_core::{
    error::{Error, SandboxErrorKind},
    sandbox::SandboxError,
};

pub use config::{DefaultCapabilities, SandboxRunConfig};
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
