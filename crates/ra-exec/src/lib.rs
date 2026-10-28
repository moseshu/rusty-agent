//! # `ra-exec`
//!
//! Process execution, background jobs, and sandbox isolation.
//!
//! **Boundary**: it runs commands and security policies that are already fully formed. It does not
//! decide whether a tool should be called and carries no product default permissions; approval
//! policy belongs to the runtime and product assembly, command content to the caller.
//!
//! **Stability**: `SandboxBackend` is an extension trait and is `Stable`; sandbox policy and
//! manifest fields are `Evolving` (platform backends will be added).

pub mod command;
pub mod filesystem;
pub mod fs;
pub mod job;
pub mod output;
pub mod process_group;
pub mod sandbox;
pub mod session;
pub mod tmpdir;

/// Default schema version for process execution structures.
pub const EXEC_SCHEMA_VERSION: ra_core::compat::SchemaVersion =
    ra_core::compat::SchemaVersion::new(1);

mod native_process;

#[allow(unsafe_code)]
pub mod shell_detect;

/// Native managed preferences and system-directory discovery.
pub mod platform_config;

/// Trusted installed helper discovery for automatic product work.
pub mod system_commands;

/// Safe native process operations shared by product command consumers.
mod background_command;
/// Native process ownership, cancellation and platform process-tree cleanup.
pub mod process {
    pub use crate::background_command::background_command;
    #[cfg(windows)]
    pub use crate::native_process::JobObject;
    #[cfg(unix)]
    pub use crate::native_process::ProcessMode;
    pub use crate::native_process::{Child, Command};
    #[cfg(unix)]
    pub use crate::process_group::kill_process_group;
}
