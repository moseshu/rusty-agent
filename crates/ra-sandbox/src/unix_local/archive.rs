//! Streaming the workspace out as a tar, and back in.
//!
//! This is how a workspace moves: a snapshot writes one of these somewhere durable, a resume reads
//! it back, and a session on another backend can be handed the same bytes. Member paths are
//! workspace-relative — `./src/main.rs`, never the host directory a provider happened to pick — so
//! an archive taken here extracts into a container without carrying this machine's layout with it.
//!
//! # Extraction is the dangerous direction
//!
//! An archive is untrusted input: it decides where its own members land. A member named `../../` or
//! one that is a symlink to `/etc` is how an extractor is made to write outside the directory it was
//! pointed at. The shared extractor in [`crate::tar_utils`] checks the whole archive before the first
//! byte is written, so a refusal leaves the workspace untouched instead of half-written.
//!
//! No size or member-count limit applies here, as none applies in the reference: limits belong to
//! an archive a caller hands to `extract`, and a restore reads back an archive a snapshot wrote.

use std::collections::BTreeSet;
use std::path::Path;

use ra_core::sandbox::{PosixPath, SandboxError};

use crate::tar_utils::{SafeExtractError, safe_extract_tarfile};

/// Writes the workspace into a tar stream.
///
/// # Errors
///
/// Returns [`ra_core::sandbox::ErrorCode::WorkspaceArchiveReadError`] when the workspace root is
/// missing or cannot be walked.
pub(crate) fn persist(root: &Path, skip: &BTreeSet<PosixPath>) -> Result<Vec<u8>, SandboxError> {
    let rendered = root.to_string_lossy().into_owned();
    if !root.exists() {
        return Err(SandboxError::workspace_archive_read(&rendered)
            .with_context("reason", "workspace_root_not_found"));
    }

    let mut builder = tar::Builder::new(Vec::new());
    builder.follow_symlinks(false);
    let failure = |error: &std::io::Error| {
        SandboxError::workspace_archive_read(&rendered).with_context("os_error", error.to_string())
    };
    builder
        .append_dir(Path::new("."), root)
        .map_err(|error| failure(&error))?;
    append_children(&mut builder, root, Path::new(""), skip).map_err(|error| failure(&error))?;
    builder.into_inner().map_err(|error| failure(&error))
}

/// Walks one directory, adding what is not excluded.
fn append_children(
    builder: &mut tar::Builder<Vec<u8>>,
    directory: &Path,
    relative: &Path,
    skip: &BTreeSet<PosixPath>,
) -> std::io::Result<()> {
    let mut children: Vec<_> = std::fs::read_dir(directory)?.collect::<Result<Vec<_>, _>>()?;
    // Sorted so two archives of the same workspace are the same bytes, which is what makes a
    // fingerprint comparison mean anything.
    children.sort_by_key(std::fs::DirEntry::file_name);

    for child in children {
        let name = child.file_name();
        let child_relative = relative.join(&name);
        if is_skipped(&child_relative, skip) {
            continue;
        }
        let archive_name = Path::new(".").join(&child_relative);
        let path = child.path();
        let metadata = path.symlink_metadata()?;

        if metadata.file_type().is_symlink() {
            let target = std::fs::read_link(&path)?;
            let mut header = tar::Header::new_gnu();
            header.set_metadata(&metadata);
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_size(0);
            builder.append_link(&mut header, &archive_name, &target)?;
        } else if metadata.is_dir() {
            builder.append_dir(&archive_name, &path)?;
            append_children(builder, &path, &child_relative, skip)?;
        } else if metadata.is_file() {
            let mut file = std::fs::File::open(&path)?;
            builder.append_file(&archive_name, &mut file)?;
        }
        // Anything else — a socket, a device, a fifo — is not workspace content and is left out
        // rather than archived as something it is not.
    }
    Ok(())
}

/// Whether a workspace-relative path is one the manifest asked not to persist.
fn is_skipped(relative: &Path, skip: &BTreeSet<PosixPath>) -> bool {
    let rendered = PosixPath::coerce(&relative.to_string_lossy());
    skip.iter().any(|prefix| rendered.is_under(prefix))
}

/// Replaces the workspace with the contents of a tar stream.
///
/// The reference's `safe_extract_tarfile`, run as its local backend runs it: symlink members are
/// restored, but only when their targets stay inside the archive. The extraction itself is the
/// shared one in [`crate::tar_utils`]; this names its failures the way this backend reports them.
///
/// # Errors
///
/// Returns [`ra_core::sandbox::ErrorCode::WorkspaceArchiveWriteError`]: with the reference's
/// `reason` and `member` in its context when a member is refused, and with `os_error` when the
/// archive cannot be read or a member cannot be written.
pub(crate) fn hydrate(root: &Path, data: &[u8]) -> Result<(), SandboxError> {
    let rendered = root.to_string_lossy().into_owned();
    safe_extract_tarfile(data, root, false).map_err(|error| {
        let refused = SandboxError::workspace_archive_write(&rendered);
        match error {
            SafeExtractError::Unsafe(member) => refused
                .with_context("reason", member.reason())
                .with_context("member", member.member()),
            SafeExtractError::Io(error) => refused
                .with_context("os_error", error.to_string())
                .with_cause(error),
            SafeExtractError::Resolve(error) => refused.with_sandbox_cause(error),
        }
    })
}
