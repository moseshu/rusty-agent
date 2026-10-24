// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! The lexical path operations the permission transforms use: the parts of Codex's `PathUri`
//! (`utils/path-uri/src/lib.rs`) and `AbsolutePathBuf::from_absolute_path`
//! (`utils/absolute-path/src/lib.rs`) they rely on, over native POSIX paths.
//!
//! Resolved paths are absolute and lexically normal: `.` is dropped and `..` removes the component
//! before it without leaving the root. The file system is never consulted, so a path need not exist
//! and symbolic links are kept as written.

use std::{
    cell::RefCell,
    io,
    marker::PhantomData,
    path::{Component, Path, PathBuf},
    rc::Rc,
};

thread_local! {
    static ABSOLUTE_PATH_BASE: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
    static ABSOLUTE_PATH_HOME: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

/// The base for deserializing permission paths: Codex's `AbsolutePathBufGuard`.
///
/// Hold this guard during synchronous deserialization on the current thread. Without it only
/// absolute paths may be read, even when a relative spelling starts with `~`. Dropping the guard
/// clears the base, as upstream does. The guard cannot move between threads because its state is
/// thread-local.
#[must_use]
#[non_exhaustive]
pub struct AbsolutePathBufGuard {
    thread_bound: PhantomData<Rc<()>>,
}

impl AbsolutePathBufGuard {
    /// Sets `base_path` as the base for paths read on this thread until the guard is dropped.
    pub fn new(base_path: &Path) -> Self {
        ABSOLUTE_PATH_BASE.with(|cell| *cell.borrow_mut() = Some(base_path.to_path_buf()));
        Self {
            thread_bound: PhantomData,
        }
    }

    /// Reads the base, requiring it for non-absolute input before home expansion.
    ///
    /// # Errors
    ///
    /// Returns Codex's error if `path` is relative and no base was set.
    pub fn deserialization_base(path: &Path) -> Result<Option<PathBuf>, &'static str> {
        let base = ABSOLUTE_PATH_BASE.with(|cell| cell.borrow().clone());
        if base.is_none() && !path.is_absolute() {
            return Err("AbsolutePathBuf deserialized without a base path");
        }
        Ok(base)
    }

    /// The home override on this thread, or the host home when none was supplied.
    #[must_use]
    pub fn home_directory() -> Option<PathBuf> {
        ABSOLUTE_PATH_HOME
            .with(|cell| cell.borrow().clone())
            .or_else(std::env::home_dir)
    }

    /// Expands home-relative paths using `home_directory` during synchronous `operation`.
    /// Restores the previous override afterwards, including on panic.
    pub fn with_home_directory<T>(home_directory: &Path, operation: impl FnOnce() -> T) -> T {
        let previous_home =
            ABSOLUTE_PATH_HOME.with(|cell| cell.replace(Some(home_directory.to_path_buf())));
        let _guard = HomeDirectoryGuard(previous_home);
        operation()
    }
}

impl Drop for AbsolutePathBufGuard {
    fn drop(&mut self) {
        ABSOLUTE_PATH_BASE.with(|cell| *cell.borrow_mut() = None);
    }
}

struct HomeDirectoryGuard(Option<PathBuf>);

impl Drop for HomeDirectoryGuard {
    fn drop(&mut self) {
        ABSOLUTE_PATH_HOME.with(|cell| *cell.borrow_mut() = self.0.take());
    }
}

/// `path` normalized lexically, or `None` if it is not absolute or contains a NUL character.
pub(crate) fn normalize(path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() || path.as_os_str().as_encoded_bytes().contains(&0) {
        return None;
    }
    let mut normalized = PathBuf::new();
    let mut depth = 0_usize;
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if depth > 0 {
                    normalized.pop();
                    depth -= 1;
                }
            }
            Component::Normal(name) => {
                normalized.push(name);
                depth += 1;
            }
        }
    }
    Some(normalized)
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
    let expanded = expand_home_directory(path);
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir()?.join(expanded)
    };
    normalize(&absolute).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("path is not absolute: {}", path.display()),
        )
    })
}

/// Expands a leading home-relative spelling using the current thread's home override.
pub(crate) fn expand_home_directory(path: &Path) -> PathBuf {
    if let Some(path_str) = path.to_str()
        && let Some(rest) = path_str.strip_prefix('~')
        && let Some(home) = AbsolutePathBufGuard::home_directory()
    {
        if rest.is_empty() {
            return home;
        } else if let Some(rest) = rest.strip_prefix('/') {
            return home.join(rest.trim_start_matches('/'));
        } else if cfg!(windows)
            && let Some(rest) = rest.strip_prefix('\\')
        {
            return home.join(rest.trim_start_matches('\\'));
        }
    }
    path.to_path_buf()
}
