//! # `ra-sandbox`
//!
//! The implementations behind the sandbox protocol: backends that really make a workspace, run
//! things in it and hand it back.
//!
//! **Stability**: `Evolving`. Backends are being carried over from the reference one at a time, and
//! the surface grows with them.
//!
//! **Boundary**: `ra-core::sandbox` says what a session is; this crate says how one is made. A
//! caller that only needs to name a sandbox depends on the kernel; a caller that needs one to exist
//! depends on this.
//!
//! # This is not `ra-exec`'s sandbox
//!
//! `ra-exec` confines a single command with an operating-system fence chosen by a host that already
//! has a workspace. A session here *is* the workspace: it outlives a command, holds files, and can
//! be stopped and resumed. The local backend does use a host fence where the reference does, but
//! that is one step inside it, not the same thing.

pub mod archive;
pub mod host_paths;
pub mod listing;
#[cfg(unix)]
pub mod materialize;
pub mod mounts;
pub mod runtime_helpers;
pub mod shell;
pub mod snapshot;
pub mod unix_local;
