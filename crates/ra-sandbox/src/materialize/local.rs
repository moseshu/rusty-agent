//! Reading the host side of a copied entry, without ever following a symlink.
//!
//! A `local_file` or `local_dir` entry names something on the machine running the SDK and has it
//! copied into the workspace. That makes the source an authorization decision: whatever the copy
//! reads is about to exist inside the sandbox, so a symlink pointing out of the source tree would
//! hand over a file the manifest never named.
//!
//! # Why the walk is pinned to directory handles
//!
//! Checking a path and then opening it by name are two different questions when anything else on
//! the machine can write to the tree in between: the check can pass on a directory that is replaced
//! by a symlink before the open happens. So the source is walked one component at a time with
//! `openat`, each step opened `O_NOFOLLOW` relative to the handle of the step before it, and every
//! entry stat-ed without following links. A symlink anywhere in the source — including one that
//! appeared mid-copy — is refused rather than resolved, and a component that changed type between
//! listing and opening is reported as having changed rather than read.
//!
//! The reference keeps a second, unpinned implementation for platforms without `openat`. It has no
//! counterpart here: this backend is compiled for unix only, where the pinned path always applies.

use std::ffi::{CString, OsStr, OsString};
use std::fs::File;
use std::io::Read;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use ra_core::sandbox::{SandboxError, SandboxPathGrant, SandboxResult};
use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags, Stat};

use crate::host_paths::sandbox_path_grant_host_path;

use super::errors::{local_checksum, local_dir_read, local_file_read};

/// What a directory is opened with: read-only, a directory, and not through a link.
const DIR_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// What a file is opened with: read-only, and not through a link.
const FILE_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// One host source a manifest entry copies from.
pub(crate) struct LocalSource<'a> {
    /// Where a relative `src` is measured from — the directory the SDK process is running in.
    base_dir: &'a Path,
    /// The source as the manifest wrote it.
    src: &'a Path,
    /// The paths outside `base_dir` this manifest is allowed to read from.
    grants: &'a [SandboxPathGrant],
}

impl<'a> LocalSource<'a> {
    /// Names a source, without touching the filesystem yet.
    pub(crate) const fn new(
        base_dir: &'a Path,
        src: &'a Path,
        grants: &'a [SandboxPathGrant],
    ) -> Self {
        Self {
            base_dir,
            src,
            grants,
        }
    }

    /// The source as an absolute path, having checked the manifest is allowed to read it.
    ///
    /// Inside `base_dir` needs no permission — that is the directory the manifest was written
    /// against. Anywhere else needs a path grant, which is the same authority that decides what the
    /// running sandbox may reach.
    ///
    /// # Errors
    ///
    /// Returns [`ra_core::sandbox::ErrorCode::LocalDirReadError`] with `reason=outside_base_dir`
    /// when nothing grants the source.
    pub(crate) fn authorized_path(&self) -> SandboxResult<PathBuf> {
        let src_input = absolute_without_symlink_resolution(&self.base_dir.join(self.src))?;
        let base = absolute_without_symlink_resolution(self.base_dir)?;
        if src_input.starts_with(&base) {
            return Ok(src_input);
        }
        for grant in self.grants {
            if src_input.starts_with(sandbox_path_grant_host_path(grant)?) {
                return Ok(src_input);
            }
        }

        let mut error = local_dir_read(&src_input)
            .with_context("reason", "outside_base_dir")
            .with_context("base_dir", base.to_string_lossy().as_ref());
        if !self.grants.is_empty() {
            error = error.with_context(
                "extra_path_grants",
                self.grants
                    .iter()
                    .map(|grant| grant.path().to_owned())
                    .collect::<Vec<_>>(),
            );
        }
        Err(error)
    }

    /// The source root, having checked that no component of the way there is a symlink.
    ///
    /// # Errors
    ///
    /// Returns [`ra_core::sandbox::ErrorCode::LocalDirReadError`] with `reason=path_not_found` for
    /// a component that is not there and `reason=symlink_not_supported` for one that is a link.
    pub(crate) fn resolve_root(&self) -> SandboxResult<PathBuf> {
        let src_input = self.authorized_path()?;
        for current in self.source_prefixes() {
            let metadata = match std::fs::symlink_metadata(&current) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Err(local_dir_read(&src_input).with_context("reason", "path_not_found"));
                }
                Err(error) => {
                    return Err(local_dir_read(&current)
                        .with_context("os_error", error.to_string())
                        .with_cause(error));
                }
            };
            if metadata.file_type().is_symlink() {
                return Err(local_dir_read(&src_input)
                    .with_context("reason", "symlink_not_supported")
                    .with_context("child", self.child_label(&current)));
            }
        }
        Ok(src_input)
    }

    /// Every prefix of the source, from the directory it starts in down to the source itself.
    fn source_prefixes(&self) -> Vec<PathBuf> {
        let (mut current, parts) = self.walk_start();
        if parts.is_empty() {
            return vec![current];
        }
        let mut prefixes = Vec::with_capacity(parts.len());
        for part in parts {
            current = current.join(part);
            prefixes.push(current.clone());
        }
        prefixes
    }

    /// Where a walk of the source begins, and the named steps it then takes.
    fn walk_start(&self) -> (PathBuf, Vec<OsString>) {
        if self.src.is_absolute() {
            (PathBuf::from("/"), named_components(self.src))
        } else {
            (self.base_dir.to_path_buf(), named_components(self.src))
        }
    }

    /// How a refused component is named in the failure: relative to `base_dir` when it is under it.
    fn child_label(&self, current: &Path) -> String {
        current
            .strip_prefix(self.base_dir)
            .unwrap_or(current)
            .to_string_lossy()
            .into_owned()
    }

    /// Opens the source root, walking to it one component at a time.
    ///
    /// # Errors
    ///
    /// As [`Self::resolve_root`], plus the open failures of any component along the way.
    fn open_root(&self, src_root: &Path) -> SandboxResult<OwnedFd> {
        // Re-checked on every open rather than once per entry: the authority question is asked
        // against the source as it is now, and a grant resolved through a symlink can stop applying
        // between one file of a directory and the next.
        self.authorized_path()?;

        let (start, parts) = self.walk_start();
        let mut current = rustix::fs::open(&start, DIR_FLAGS, Mode::empty()).map_err(|error| {
            if error == rustix::io::Errno::NOENT {
                local_dir_read(src_root).with_context("reason", "path_changed_during_copy")
            } else {
                local_dir_read(src_root).with_context("os_error", error.to_string())
            }
        })?;

        let mut walked = PathBuf::new();
        for part in parts {
            walked.push(&part);
            let name = file_name(&part)?;
            let next =
                rustix::fs::openat(&current, &name, DIR_FLAGS, Mode::empty()).map_err(|error| {
                    open_failure(
                        src_root,
                        &current,
                        &name,
                        &walked,
                        FileType::Directory,
                        error,
                    )
                })?;
            if !is(&rustix::fs::fstat(&next), FileType::Directory) {
                return Err(local_dir_read(src_root)
                    .with_context("reason", "path_changed_during_copy")
                    .with_context("child", walked.to_string_lossy().as_ref()));
            }
            current = next;
        }
        Ok(current)
    }

    /// Every regular file under the source root, relative to it.
    ///
    /// # Errors
    ///
    /// As [`Self::open_root`], and for a symlink anywhere in the tree.
    pub(crate) fn list_files(&self, src_root: &Path) -> SandboxResult<Vec<PathBuf>> {
        let root = self.open_root(src_root)?;
        let mut files = Vec::new();
        list_from_dir(src_root, &root, Path::new(""), &mut files)?;
        // The walk reads each directory in whatever order the filesystem hands it back, and the
        // copy is reported as a receipt somebody compares between runs.
        files.sort();
        Ok(files)
    }

    /// Opens one file under the source root, walking to it one component at a time.
    ///
    /// # Errors
    ///
    /// As [`Self::open_root`], plus `path_changed_during_copy` for a leaf that is no longer a
    /// regular file and `symlink_not_supported` for one that became a link.
    pub(crate) fn open_file(&self, src_root: &Path, rel_child: &Path) -> SandboxResult<File> {
        let mut current = self.open_root(src_root)?;
        let mut walked = PathBuf::new();

        let components = named_components(rel_child);
        let Some((leaf, parents)) = components.split_last() else {
            return Err(
                local_file_read(src_root).with_context("reason", "path_changed_during_copy")
            );
        };
        for part in parents {
            walked.push(part);
            let name = file_name(part)?;
            let next =
                rustix::fs::openat(&current, &name, DIR_FLAGS, Mode::empty()).map_err(|error| {
                    open_failure(
                        src_root,
                        &current,
                        &name,
                        &walked,
                        FileType::Directory,
                        error,
                    )
                })?;
            if !is(&rustix::fs::fstat(&next), FileType::Directory) {
                return Err(local_dir_read(src_root)
                    .with_context("reason", "path_changed_during_copy")
                    .with_context("child", rel_child.to_string_lossy().as_ref()));
            }
            current = next;
        }

        walked.push(leaf);
        let name = file_name(leaf)?;
        let opened =
            rustix::fs::openat(&current, &name, FILE_FLAGS, Mode::empty()).map_err(|error| {
                open_failure(
                    src_root,
                    &current,
                    &name,
                    &walked,
                    FileType::RegularFile,
                    error,
                )
            })?;
        if !is(&rustix::fs::fstat(&opened), FileType::RegularFile) {
            return Err(local_dir_read(src_root)
                .with_context("reason", "path_changed_during_copy")
                .with_context("child", rel_child.to_string_lossy().as_ref()));
        }
        Ok(File::from(opened))
    }
}

/// The absolute form of a declared source, for a refusal to quote.
///
/// Falls back to the plain join when this process has no readable working directory, because a
/// refusal that lost the path it was about is worse than one that quotes a relative path.
pub(crate) fn absolute_source(base_dir: &Path, src: &Path) -> PathBuf {
    let joined = base_dir.join(src);
    absolute_without_symlink_resolution(&joined).unwrap_or(joined)
}

/// Reads a file and hashes what was read, so the receipt describes the bytes that were copied.
///
/// # Errors
///
/// Returns [`ra_core::sandbox::ErrorCode::LocalChecksumError`] when the open file cannot be read.
pub(crate) fn read_and_hash(mut file: File, src: &Path) -> SandboxResult<(Vec<u8>, String)> {
    use sha2::{Digest, Sha256};

    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| local_checksum(src).with_cause(error))?;
    let digest = Sha256::digest(&bytes);
    Ok((bytes, format!("{digest:x}")))
}

/// Collects the regular files under one already-open directory.
fn list_from_dir(
    src_root: &Path,
    dir_fd: &OwnedFd,
    rel_dir: &Path,
    files: &mut Vec<PathBuf>,
) -> SandboxResult<()> {
    let entries = Dir::read_from(dir_fd)
        .map_err(|error| local_dir_read(src_root).with_context("os_error", error.to_string()))?;
    for entry in entries {
        let entry = entry.map_err(|error| {
            local_dir_read(src_root).with_context("os_error", error.to_string())
        })?;
        let name = entry.file_name().to_owned();
        if name.as_bytes() == b"." || name.as_bytes() == b".." {
            continue;
        }
        let rel_child = rel_dir.join(OsStr::from_bytes(name.as_bytes()));

        // Stat-ed rather than trusting the directory entry's own type: several filesystems report
        // it as unknown, and "unknown" read as "not a symlink" is the one mistake this walk exists
        // to prevent.
        let metadata = match rustix::fs::statat(dir_fd, &name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(metadata) => metadata,
            Err(rustix::io::Errno::NOENT) => {
                return Err(changed_during_copy(src_root, &rel_child));
            }
            Err(error) => {
                return Err(local_dir_read(src_root).with_context("os_error", error.to_string()));
            }
        };
        match FileType::from_raw_mode(metadata.st_mode) {
            FileType::Symlink => return Err(symlink_refused(src_root, &rel_child)),
            FileType::RegularFile => files.push(rel_child),
            FileType::Directory => {
                let child = rustix::fs::openat(dir_fd, &name, DIR_FLAGS, Mode::empty()).map_err(
                    |error| {
                        open_failure(
                            src_root,
                            dir_fd,
                            &name,
                            &rel_child,
                            FileType::Directory,
                            error,
                        )
                    },
                )?;
                if !is(&rustix::fs::fstat(&child), FileType::Directory) {
                    return Err(changed_during_copy(src_root, &rel_child));
                }
                list_from_dir(src_root, &child, &rel_child, files)?;
            }
            // A socket, a device or a fifo is not content a workspace copy can carry, and the
            // reference passes over them rather than refusing the whole directory.
            _ => {}
        }
    }
    Ok(())
}

/// Explains an open that failed, by looking at what is actually at that name now.
fn open_failure(
    src_root: &Path,
    parent: &OwnedFd,
    name: &CString,
    rel_child: &Path,
    expected: FileType,
    error: rustix::io::Errno,
) -> SandboxError {
    match rustix::fs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(metadata) => {
            let actual = FileType::from_raw_mode(metadata.st_mode);
            if actual == FileType::Symlink {
                return symlink_refused(src_root, rel_child);
            }
            if actual != expected {
                return changed_during_copy(src_root, rel_child);
            }
        }
        Err(rustix::io::Errno::NOENT) => return changed_during_copy(src_root, rel_child),
        Err(_) => {}
    }
    if error == rustix::io::Errno::LOOP {
        return symlink_refused(src_root, rel_child);
    }
    local_dir_read(src_root)
        .with_context("child", rel_child.to_string_lossy().as_ref())
        .with_context("os_error", error.to_string())
}

/// Refuses a source component that is a symlink.
fn symlink_refused(src_root: &Path, rel_child: &Path) -> SandboxError {
    local_dir_read(src_root)
        .with_context("reason", "symlink_not_supported")
        .with_context("child", rel_child.to_string_lossy().as_ref())
}

/// Refuses a source component that stopped being what the walk found it to be.
fn changed_during_copy(src_root: &Path, rel_child: &Path) -> SandboxError {
    local_dir_read(src_root)
        .with_context("reason", "path_changed_during_copy")
        .with_context("child", rel_child.to_string_lossy().as_ref())
}

/// Whether a stat that may have failed describes the expected kind of thing.
fn is(metadata: &rustix::io::Result<Stat>, expected: FileType) -> bool {
    metadata
        .as_ref()
        .is_ok_and(|metadata| FileType::from_raw_mode(metadata.st_mode) == expected)
}

/// Renders one path component as the C string an `openat` takes.
fn file_name(part: &OsStr) -> SandboxResult<CString> {
    CString::new(part.as_bytes()).map_err(|error| {
        local_dir_read(Path::new(part))
            .with_context("reason", "path_changed_during_copy")
            .with_cause(error)
    })
}

/// Makes a path absolute by text alone, resolving `..` without asking the filesystem anything.
///
/// The port of the reference's `os.path.abspath`, and the "without" is the point: resolving a `..`
/// through a symlink would answer a question about where the link points rather than about the path
/// the manifest wrote, and the walk that follows is the thing entitled to touch the filesystem.
///
/// # Errors
///
/// Returns [`ra_core::sandbox::ErrorCode::LocalDirReadError`] when a relative path has to be
/// measured from a working directory this process cannot read.
fn absolute_without_symlink_resolution(path: &Path) -> SandboxResult<PathBuf> {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| local_dir_read(path).with_cause(error))?
            .join(path)
    };

    let mut normalized = PathBuf::from("/");
    for component in joined.components() {
        match component {
            Component::RootDir | Component::Prefix(_) => normalized = PathBuf::from("/"),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(name) => normalized.push(name),
        }
    }
    Ok(normalized)
}

/// The named steps of a path: `.` drops out, `..` is a step like any other.
fn named_components(path: &Path) -> Vec<OsString> {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name.to_owned()),
            Component::ParentDir => Some(OsString::from("..")),
            Component::RootDir | Component::CurDir | Component::Prefix(_) => None,
        })
        .collect()
}
