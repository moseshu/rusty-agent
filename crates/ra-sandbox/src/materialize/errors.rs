//! The refusals materialization raises, with the reference's wording and context keys.
//!
//! The error codes are modelled in `ra_core::sandbox`, and so is the retryability each one defaults
//! to. What lives here is the message and the context a materializing backend attaches, which the
//! kernel has no constructor for because it never materializes anything itself.

use std::path::Path;

use ra_core::sandbox::{ErrorCode, OpName, SandboxError};

/// A host file that could not be read into the workspace.
pub(crate) fn local_file_read(src: &Path) -> SandboxError {
    SandboxError::new(
        ErrorCode::LocalFileReadError,
        OpName::Materialize,
        format!("failed to read local file artifact: {}", src.display()),
    )
    .with_context("src", src.to_string_lossy().as_ref())
}

/// A host directory that could not be read into the workspace.
pub(crate) fn local_dir_read(src: &Path) -> SandboxError {
    SandboxError::new(
        ErrorCode::LocalDirReadError,
        OpName::Materialize,
        format!("failed to read local dir artifact: {}", src.display()),
    )
    .with_context("src", src.to_string_lossy().as_ref())
}

/// A host file that was read but could not be hashed.
pub(crate) fn local_checksum(src: &Path) -> SandboxError {
    SandboxError::new(
        ErrorCode::LocalChecksumError,
        OpName::Materialize,
        format!("failed to checksum local artifact: {}", src.display()),
    )
    .with_context("src", src.to_string_lossy().as_ref())
}

/// A sandbox with no `git`, asked for a checkout.
pub(crate) fn git_missing(repo: &str, reference: &str) -> SandboxError {
    SandboxError::new(
        ErrorCode::GitMissingInImage,
        OpName::Materialize,
        "git is required in the container image to materialize git_repo artifacts",
    )
    .with_context("repo", repo)
    .with_context("ref", reference)
}

/// A checkout that could not be fetched.
pub(crate) fn git_clone(url: &str, reference: &str, stderr: &str) -> SandboxError {
    SandboxError::new(
        ErrorCode::GitCloneError,
        OpName::Materialize,
        format!("git clone failed for {url}@{reference}"),
    )
    .with_context("url", url)
    .with_context("ref", reference)
    .with_context("stderr", stderr)
}

/// A checkout whose `subpath` does not name somewhere inside the repository.
pub(crate) fn git_subpath(repo: &str, subpath: &str, reason: &'static str) -> SandboxError {
    SandboxError::new(
        ErrorCode::GitSubpathError,
        OpName::Materialize,
        format!("git repo subpath must be a relative path inside the repository: {subpath}"),
    )
    .with_context("repo", repo)
    .with_context("subpath", subpath)
    .with_context("reason", reason)
}

/// A checkout that was fetched but could not be copied into the workspace.
pub(crate) fn git_copy(src_root: &str, dest: &str, stderr: &str) -> SandboxError {
    SandboxError::new(
        ErrorCode::GitCopyError,
        OpName::Materialize,
        "copy from git repo failed",
    )
    .with_context("src_root", src_root)
    .with_context("dest", dest)
    .with_context("stderr", stderr)
}

/// Something a manifest may declare that this implementation cannot yet put in a workspace.
///
/// Refused rather than skipped: a workspace that silently came up without the content its manifest
/// declared is the failure mode an empty receipt cannot describe.
pub(crate) fn unsupported_entry(entry_type: &str, detail: &str) -> SandboxError {
    SandboxError::new(
        ErrorCode::SandboxConfigInvalid,
        OpName::Materialize,
        format!("cannot materialize a `{entry_type}` entry: {detail}"),
    )
    .with_context("entry_type", entry_type)
}
