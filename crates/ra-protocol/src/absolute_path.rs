//! Protocol adapters for shared native absolute paths and synchronous home overrides.

use std::{
    io,
    path::{Path, PathBuf},
};

/// Resolves a path using the shared native constructor and current home scope.
pub(crate) fn from_absolute_path(path: &Path) -> io::Result<PathBuf> {
    ra_paths::AbsolutePathBuf::from_absolute_path(path)
        .map(ra_paths::AbsolutePathBuf::into_path_buf)
}

/// Accepts only input that is absolute after home and namespace normalization.
pub(crate) fn from_absolute_path_checked(path: &Path) -> io::Result<PathBuf> {
    ra_paths::AbsolutePathBuf::from_absolute_path_checked(path)
        .map(ra_paths::AbsolutePathBuf::into_path_buf)
}
