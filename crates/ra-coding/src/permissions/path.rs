// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! The lexical path operations the permission transforms use: the parts of Codex's `PathUri`
//! (`utils/path-uri/src/lib.rs`) and `AbsolutePathBuf::from_absolute_path`
//! (`utils/absolute-path/src/lib.rs`) they rely on, over native POSIX paths.
//!
//! Resolved paths are absolute and lexically normal: `.` is dropped and `..` removes the component
//! before it without leaving the root. The file system is never consulted, so a path need not exist
//! and symbolic links are kept as written.

use std::{
    io,
    path::{Component, Path, PathBuf},
};

pub use ra_paths::AbsolutePathBufGuard;
pub(crate) use ra_paths::expand_home_directory;

/// `path` normalized lexically, or `None` if it is not absolute or contains a NUL character.
pub(crate) fn normalize(path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() || path.as_os_str().as_encoded_bytes().contains(&0) {
        return None;
    }
    ra_paths::AbsolutePathBuf::from_absolute_path_checked(path)
        .ok()
        .map(ra_paths::AbsolutePathBuf::into_path_buf)
}

/// `path` resolved against `base`: Codex's `PathUri::join`. An absolute `path` replaces `base`;
/// a relative one is appended lexically. A path containing a NUL character cannot be resolved.
pub(crate) fn join(base: &Path, path: &str) -> Option<PathBuf> {
    if path.contains('\0') {
        return None;
    }
    if path.is_empty() {
        return normalize(base);
    }
    let path = Path::new(path);
    if path.is_absolute() {
        normalize(path)
    } else {
        normalize(&base.join(path))
    }
}

/// How many named components `path` has below the root: Codex's `PathUri::lexical_depth`.
pub(crate) fn lexical_depth(path: &Path) -> usize {
    path.components()
        .filter(|component| matches!(component, Component::Normal(_)))
        .count()
}

/// Whether the subtrees rooted at `left` and `right` overlap: Codex's `PathUri::overlaps`.
pub(crate) fn overlaps(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

/// The root of the file system `cwd` is on: Codex's `file_system_root`.
pub(crate) fn file_system_root(cwd: &Path) -> Option<PathBuf> {
    normalize(cwd)?.ancestors().last().map(Path::to_path_buf)
}

/// A serialized path resolved using Codex's explicit deserialization base and home overrides.
pub(crate) fn deserialize_absolute_path(path: &Path) -> io::Result<PathBuf> {
    let base = AbsolutePathBufGuard::deserialization_base(path)
        .map_err(|message| io::Error::new(io::ErrorKind::InvalidInput, message))?;
    match base {
        Some(base) => {
            let expanded = expand_home_directory(path);
            normalize(&base.join(expanded)).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "permission path cannot be resolved",
                )
            })
        }
        None => from_absolute_path(path),
    }
}

/// `path` made absolute and normal: Codex's `AbsolutePathBuf::from_absolute_path`. A leading `~`
/// is the home directory, and a relative path is resolved against the current directory.
pub(crate) fn from_absolute_path(path: &Path) -> io::Result<PathBuf> {
    let absolute = ra_paths::AbsolutePathBuf::from_absolute_path(path)?;
    normalize(absolute.as_path()).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("path is not absolute: {}", path.display()),
        )
    })
}
