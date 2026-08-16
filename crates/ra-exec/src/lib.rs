//! # `ra-exec`
//!
//! Process execution, PTY, background jobs, sandbox isolation.
//!
//! **Boundary**: it runs commands and security policies that are already fully formed. It does not
//! decide whether a tool should be called and carries no product default permissions; approval
//! policy belongs to the runtime and product assembly, command content to the caller.
//!
//! **Stability**: `SandboxBackend` is an extension trait and is `Stable`; sandbox policy and
//! manifest fields are `Evolving` (platform backends will be added).

pub mod command;
pub mod fs;
pub mod job;
pub mod output;
pub mod pty;
pub mod sandbox;
pub mod session;

/// Default schema version for process execution structures.
pub const EXEC_SCHEMA_VERSION: ra_core::compat::SchemaVersion =
    ra_core::compat::SchemaVersion::new(1);
