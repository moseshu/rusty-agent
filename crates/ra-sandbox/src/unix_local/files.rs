//! The workspace as a directory: listing, creating, removing, reading and writing.
//!
//! Every operation here acts on the host filesystem directly, because for this backend that *is*
//! the workspace. The path it acts on has already been through the resolving policy, so a symlink
//! that points out of the workspace has already been refused.
//!
//! # Running as another account is a check, not a channel
//!
//! When a caller names a user, the reference does not do the work through `sudo`; it asks, as that
//! user, whether the operation would be permitted, and then does it locally. The shell scripts that
//! ask are here. Doing the work through `sudo` instead would mean the SDK could write files the
//! account running it cannot read back, which is a workspace only half of the process can use.

use std::path::Path;

use ra_core::sandbox::{EntryKind, ExecResult, FileEntry, Permissions, SandboxError};

pub(crate) use crate::session_scripts::{
    MKDIR_ACCESS_CHECK_SCRIPT, READ_ACCESS_CHECK_SCRIPT, READ_PATH_PROBE_SCRIPT,
    READ_PATH_PROBE_TIMEOUT_S, RM_ACCESS_CHECK_SCRIPT, diagnostic_text,
};

/// Lists a directory by reading it, rather than by running `ls`.
///
/// Symlinks are reported as symlinks instead of as whatever they point at: a caller deciding what
/// to do with an entry needs to know it is a link, and a listing that silently followed one would
/// describe a file that is not in this directory at all.
///
/// # Errors
///
/// Returns [`ra_core::sandbox::ErrorCode::ExecNonzero`] shaped as the `ls` that would have failed,
/// which is what the reference reports so that a local listing and a remote one fail alike.
pub(crate) fn list_directory(path: &Path) -> Result<Vec<FileEntry>, SandboxError> {
    let rendered = path.to_string_lossy().into_owned();
    let failure = |error: &std::io::Error| {
        SandboxError::exec_nonzero(
            ExecResult::new(Vec::new(), error.to_string().into_bytes(), 1),
            vec![
                "ls".to_owned(),
                "-la".to_owned(),
                "--".to_owned(),
                rendered.clone(),
            ],
        )
    };

    let mut listed = Vec::new();
    for entry in std::fs::read_dir(path).map_err(|error| failure(&error))? {
        let entry = entry.map_err(|error| failure(&error))?;
        let metadata = entry
            .path()
            .symlink_metadata()
            .map_err(|error| failure(&error))?;
        let file_type = metadata.file_type();
        let kind = if file_type.is_symlink() {
            EntryKind::Symlink
        } else if file_type.is_dir() {
            EntryKind::Directory
        } else if file_type.is_file() {
            EntryKind::File
        } else {
            EntryKind::Other
        };
        listed.push(
            FileEntry::new(
                entry.path().to_string_lossy().into_owned(),
                permissions_of(&metadata),
            )
            .with_ownership(owner_of(&metadata), group_of(&metadata))
            .with_size(size_of(&metadata))
            .with_kind(kind),
        );
    }
    Ok(listed)
}

/// The permission bits and directory flag a listing reports.
fn permissions_of(metadata: &std::fs::Metadata) -> Permissions {
    use std::os::unix::fs::MetadataExt;

    Permissions::from_mode(metadata.mode())
}

/// The owning account, as the numeric id the filesystem stores.
///
/// The reference reports the number rather than looking the name up, and so does this: resolving it
/// would consult this host's account database to describe a workspace that may not share it.
fn owner_of(metadata: &std::fs::Metadata) -> String {
    use std::os::unix::fs::MetadataExt;

    metadata.uid().to_string()
}

/// The owning group, as the numeric id the filesystem stores.
fn group_of(metadata: &std::fs::Metadata) -> String {
    use std::os::unix::fs::MetadataExt;

    metadata.gid().to_string()
}

/// The size a listing reports.
fn size_of(metadata: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;

    metadata.size()
}

/// Creates a directory, and its parents when asked.
///
/// An existing directory is success, as it is for the reference: materializing a manifest twice
/// must not fail the second time.
///
/// # Errors
///
/// Returns [`ra_core::sandbox::ErrorCode::WorkspaceArchiveWriteError`] when the directory could not
/// be created.
pub(crate) fn make_directory(path: &Path, parents: bool) -> Result<(), SandboxError> {
    let result = if parents {
        std::fs::create_dir_all(path)
    } else {
        match std::fs::create_dir(path) {
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && path.is_dir() => {
                Ok(())
            }
            other => other,
        }
    };
    result.map_err(|error| write_failure(path, &error))
}

/// Removes a path, and everything under it when asked.
///
/// A directory is removed as a directory and a symlink to one is removed as a link, which is the
/// difference between deleting a shortcut and deleting what it points at.
///
/// # Errors
///
/// Returns [`ra_core::sandbox::ErrorCode::ExecNonzero`] for a non-recursive removal of something
/// that is not there — a recursive one treats it as already done — and
/// [`ra_core::sandbox::ErrorCode::WorkspaceArchiveWriteError`] for any other failure.
pub(crate) fn remove(path: &Path, recursive: bool) -> Result<(), SandboxError> {
    let metadata = path.symlink_metadata();
    let is_directory = metadata
        .as_ref()
        .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink());

    let result = if is_directory {
        if recursive {
            std::fs::remove_dir_all(path)
        } else {
            std::fs::remove_dir(path)
        }
    } else {
        std::fs::remove_file(path)
    };

    match result {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if recursive {
                return Ok(());
            }
            Err(SandboxError::exec_nonzero(
                ExecResult::new(Vec::new(), error.to_string().into_bytes(), 1),
                vec![
                    "rm".to_owned(),
                    "--".to_owned(),
                    path.to_string_lossy().into_owned(),
                ],
            ))
        }
        Err(error) => Err(write_failure(path, &error)),
    }
}

/// Reads a file out of the workspace.
///
/// `requested` is the path the caller named, which is what a refusal quotes: a caller that asked
/// for `notes.md` should not be told about a provider's temporary directory.
///
/// # Errors
///
/// Returns [`ra_core::sandbox::ErrorCode::WorkspaceReadNotFound`] when the file is not there, and
/// [`ra_core::sandbox::ErrorCode::WorkspaceArchiveReadError`] for any other failure.
///
/// `max_bytes` stops the read after that many bytes from the start, so a caller with a ceiling
/// never holds more than it asked for, whatever the file's size.
pub(crate) fn read_file(
    path: &Path,
    requested: &str,
    max_bytes: Option<u64>,
) -> Result<Vec<u8>, SandboxError> {
    let failure = |error: std::io::Error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            SandboxError::workspace_read_not_found(requested).with_cause(error)
        } else {
            SandboxError::workspace_archive_read(requested)
                .with_context("os_error", error.to_string())
                .with_cause(error)
        }
    };
    let Some(max_bytes) = max_bytes else {
        return std::fs::read(path).map_err(failure);
    };
    let file = std::fs::File::open(path).map_err(failure)?;
    let mut data = Vec::new();
    std::io::Read::read_to_end(&mut std::io::Read::take(file, max_bytes), &mut data)
        .map_err(failure)?;
    Ok(data)
}

/// Writes a file into the workspace, creating the directories above it.
///
/// # Errors
///
/// Returns [`ra_core::sandbox::ErrorCode::WorkspaceArchiveWriteError`] when the file could not be
/// written.
pub(crate) fn write_file(path: &Path, data: &[u8]) -> Result<(), SandboxError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| write_failure(path, &error))?;
    }
    std::fs::write(path, data).map_err(|error| write_failure(path, &error))
}

/// Reports a workspace write that did not happen.
fn write_failure(path: &Path, error: &std::io::Error) -> SandboxError {
    SandboxError::workspace_archive_write(&path.to_string_lossy())
        .with_context("os_error", error.to_string())
}
