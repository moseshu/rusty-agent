//! The sandbox protocol: what a workspace execution environment offers, stated without choosing one.
//!
//! **Stability**: `Evolving`. The protocol is being carried over from the reference implementation
//! one layer at a time; names and fields already here are meant to match it and will not be
//! renamed for taste, but the surface is still growing.
//!
//! **Boundary**: types, traits and codes only. A backend — a local one, a container, a hosted
//! provider — implements this from a service crate. Nothing here opens a file, spawns a process or
//! knows what a container is, which is what keeps the loop kernel able to name a sandbox without
//! depending on any implementation of one.
//!
//! # This is not the process fence
//!
//! `ra-exec` also has something called a sandbox: a mechanism that wraps one command so the
//! operating system confines it. That is a per-command fence chosen by a host that already has a
//! workspace. This module is the workspace itself — an environment that outlives a command, holds
//! files, and can be stopped, serialized and resumed. The two compose, and neither substitutes for
//! the other.

pub mod error;
pub mod registry;
pub mod snapshot;
pub mod types;

pub use error::{ErrorCategory, ErrorCode, OpName, SandboxError, SandboxErrorDetails};
pub use registry::{
    DiscriminatedPayload, RegistryError, RegistryKind, TypeRegistry, client_options_kind,
    session_state_kind, snapshot_kind,
};
pub use snapshot::{NOOP_SNAPSHOT_TYPE, Snapshot};
pub use types::{
    ErrorContext, ExecResult, ExposedPortEndpoint, FileMode, Group, Permissions,
    PermissionsParseError, UnsupportedScheme, User,
};
