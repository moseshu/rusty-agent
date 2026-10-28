// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Descriptor-aware native child process operations used by filesystem helpers.
#![allow(
    unsafe_code,
    reason = "Keep the upstream native descriptor and process launch boundary isolated."
)]
mod child;
pub use child::Child;
mod child_command;
pub(crate) use child_command::ChildStdin;
pub use child_command::Command;
#[cfg(unix)]
pub(crate) use child_command::DescriptorPolicy;
#[cfg(unix)]
pub use child_command::ProcessMode;
pub(crate) use child_command::SpawnFallback;
#[cfg(target_os = "linux")]
mod linux_fds;
pub(crate) mod pipe;
mod process;
#[cfg(unix)]
pub(crate) use crate::process_group;
pub(crate) mod pty;
#[cfg(unix)]
mod unix_io;
#[cfg(windows)]
mod win;
#[cfg(windows)]
mod windows_input;

#[allow(
    dead_code,
    reason = "Retain source process-session compatibility items for the unported execution backend."
)]
pub(crate) const DEFAULT_OUTPUT_BYTES_CAP: usize = 1024 * 1024;

/// Handle for interacting with a spawned process (PTY or pipe).
pub(crate) use process::ProcessHandle;
/// Bundle of process handles plus split output and exit receivers returned by spawn helpers.
pub(crate) use process::SpawnedProcess;
/// Backwards-compatible alias for `ProcessHandle`.
#[allow(
    dead_code,
    reason = "Retain source process-session compatibility items for the unported execution backend."
)]
pub(crate) type ExecCommandSession = ProcessHandle;
/// Backwards-compatible alias for `SpawnedProcess`.
#[allow(
    dead_code,
    reason = "Retain source process-session compatibility items for the unported execution backend."
)]
pub(crate) type SpawnedPty = SpawnedProcess;
#[cfg(windows)]
pub use win::JobObject;
#[cfg(windows)]
pub(crate) use windows_input::WindowsTtyInputNormalizer;

#[cfg(target_os = "linux")]
mod spawn_helper;
#[cfg(target_os = "linux")]
mod spawn_helper_main;
