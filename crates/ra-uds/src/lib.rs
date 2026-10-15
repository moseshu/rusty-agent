//! # `ra-uds`
//!
//! Unix control socket paths shared by transports and host sandboxes.
//!
//! **Stability**: `Evolving`. **Boundary**: filesystem and socket primitives ported from Codex's
//! `uds` crate, without agent runtime, product configuration, or daemon lifecycle policy.
//!
//! The host-local rendezvous root for privileged app-server RPC sockets.
//!
//! Every listener uses this root, which sandboxes hide even before a daemon
//! starts. It must not depend on the home directory, environment overrides, or command settings.
//! Ported from Codex `uds/src/daemon_directory.rs`; the product namespace avoids sharing sockets
//! and directory ownership with an installed Codex daemon.

#![cfg(unix)]

use std::fs;
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;

/// Returns the fixed executor-local directory that every sandbox must hide.
pub fn shared_daemon_socket_directory() -> io::Result<PathBuf> {
    // Resolve the system alias /tmp -> /private/tmp on macOS.
    let temporary_root = fs::canonicalize("/tmp")?;
    let uid = rustix::process::geteuid().as_raw();
    // Keep the prefix short enough for a SHA-256 filename even with a ten-digit uid on macOS.
    Ok(temporary_root.join(format!("ra-daemon-{uid}")))
}

/// Creates the reserved directory, rejecting symlinks and unsafe existing owners or modes.
pub fn prepare_shared_daemon_socket_directory() -> io::Result<PathBuf> {
    let directory = shared_daemon_socket_directory()?;
    match fs::DirBuilder::new().mode(0o700).create(&directory) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let metadata = fs::symlink_metadata(&directory)?;
    if !metadata.is_dir()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o777 != 0o700
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "app-server socket directory must be a user-owned directory with mode 0700",
        ));
    }
    Ok(directory)
}
