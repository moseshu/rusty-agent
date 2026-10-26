// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Shared native absolute paths for configuration, permissions, manifests and protocol I/O.
//! Constructors resolve home and relative paths lexically; canonicalization helpers explicitly
//! consult the filesystem and retain logical paths through nested symbolic links.
//!
//! A leading `~` is the home directory. On Windows a `\\?\` or `\\.\` prefix on a drive or UNC
//! path is dropped. A relative path is resolved against the current directory, and the result is
//! normalized lexically: `.` is dropped, `..` removes the component before it and stops at the
//! root. Lexical constructors do not consult the filesystem, so paths need not exist and symbolic
//! links are kept.
//!
//! Configuration, permissions, manifests and execution policy share this native type and its
//! synchronous thread-local base and home scopes through [`AbsolutePathBufGuard`].

use std::{
    borrow::Cow,
    cell::RefCell,
    fmt, io,
    marker::PhantomData,
    path::{Component, Path, PathBuf},
    rc::Rc,
};

use serde::{Deserialize, Deserializer, Serialize, de::Error as _};

/// A path made absolute and normalized lexically (see the module's documentation).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, schemars::JsonSchema)]
pub struct AbsolutePathBuf(PathBuf);

impl AbsolutePathBuf {
    /// Resolves `path` against an explicit absolute base without consulting the filesystem.
    #[must_use]
    pub fn resolve_path_against_base<P: AsRef<Path>, B: AsRef<Path>>(
        path: P,
        base_path: B,
    ) -> Self {
        let expanded = expand_home_directory(path.as_ref());
        let expanded = normalize_path_for_platform(&expanded);
        let base_path = normalize_path_for_platform(base_path.as_ref());
        Self(normalize_path(&path_with_base(&expanded, &base_path)))
    }

    /// `path` made absolute and normalized. Reads the current directory only when `path` is
    /// relative.
    pub fn from_absolute_path<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let expanded = expand_home_directory(path.as_ref());
        let expanded = normalize_path_for_platform(&expanded);
        absolutize(&expanded).map(Self)
    }

    /// Accepts only paths that are absolute after home and Windows namespace normalization.
    pub fn from_absolute_path_checked<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let expanded = expand_home_directory(path.as_ref());
        let expanded = normalize_path_for_platform(&expanded);
        if !expanded.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("path is not absolute: {}", path.as_ref().display()),
            ));
        }
        Ok(Self(normalize_path(&path_with_base(
            &expanded,
            Path::new("/"),
        ))))
    }

    /// Returns the current working directory as an absolute normalized path.
    pub fn current_dir() -> io::Result<Self> {
        Self::from_absolute_path(std::env::current_dir()?)
    }

    /// Resolves relative input against the current directory.
    pub fn relative_to_current_dir<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        if path.as_ref().is_absolute() {
            return Self::from_absolute_path(path);
        }
        Ok(Self::resolve_path_against_base(
            path,
            std::env::current_dir()?,
        ))
    }

    /// Resolves `path` against this path, normalizing components lexically.
    #[must_use]
    pub fn join<P: AsRef<Path>>(&self, path: P) -> Self {
        Self::resolve_path_against_base(path, &self.0)
    }

    /// Resolves filesystem aliases and symlinks, requiring the path to exist.
    pub fn canonicalize(&self) -> io::Result<Self> {
        dunce::canonicalize(&self.0).map(Self)
    }

    /// Returns the absolute lexical parent, if this path is not a root.
    #[must_use]
    pub fn parent(&self) -> Option<Self> {
        self.0.parent().map(|path| {
            debug_assert!(
                path.is_absolute(),
                "parent of AbsolutePathBuf must be absolute"
            );
            Self(path.to_path_buf())
        })
    }

    /// Iterates over this path and its absolute lexical parents.
    pub fn ancestors(&self) -> impl Iterator<Item = Self> + '_ {
        self.0.ancestors().map(|path| {
            debug_assert!(
                path.is_absolute(),
                "ancestor of AbsolutePathBuf must be absolute"
            );
            Self(path.to_path_buf())
        })
    }

    /// The path.
    #[must_use]
    pub fn as_path(&self) -> &Path {
        &self.0
    }

    /// The path, owned.
    #[must_use]
    pub fn to_path_buf(&self) -> PathBuf {
        self.0.clone()
    }

    /// The path, taken.
    #[must_use]
    pub fn into_path_buf(self) -> PathBuf {
        self.0
    }

    /// The path for display.
    #[must_use]
    pub fn display(&self) -> std::path::Display<'_> {
        self.0.display()
    }

    /// The path as a string, with invalid UTF-8 replaced.
    #[must_use]
    pub fn to_string_lossy(&self) -> Cow<'_, str> {
        self.0.to_string_lossy()
    }
}

/// Canonicalize a path when possible, but preserve the logical absolute path
/// whenever canonicalization would rewrite it through a nested symlink.
///
/// Top-level system aliases such as macOS `/var -> /private/var` still remain
/// canonicalized so existing runtime expectations around those paths stay
/// stable. If the full path cannot be canonicalized, this returns the logical
/// absolute path; use [`canonicalize_existing_preserving_symlinks`] for paths
/// that must exist.
pub fn canonicalize_preserving_symlinks(path: &Path) -> std::io::Result<PathBuf> {
    let logical = AbsolutePathBuf::from_absolute_path(path)?.into_path_buf();
    let preserve_logical_path = should_preserve_logical_path(&logical);
    match dunce::canonicalize(path) {
        Ok(canonical) if preserve_logical_path && canonical != logical => Ok(logical),
        Ok(canonical) => Ok(canonical),
        Err(_) => Ok(logical),
    }
}

/// Canonicalize an existing path while preserving the logical absolute path
/// whenever canonicalization would rewrite it through a nested symlink.
///
/// Unlike [`canonicalize_preserving_symlinks`], canonicalization failures are
/// propagated so callers can reject invalid working directories early.
pub fn canonicalize_existing_preserving_symlinks(path: &Path) -> std::io::Result<PathBuf> {
    let logical = AbsolutePathBuf::from_absolute_path(path)?.into_path_buf();
    let canonical = dunce::canonicalize(path)?;
    if should_preserve_logical_path(&logical) && canonical != logical {
        Ok(logical)
    } else {
        Ok(canonical)
    }
}

fn should_preserve_logical_path(logical: &Path) -> bool {
    logical.ancestors().any(|ancestor| {
        let Ok(metadata) = std::fs::symlink_metadata(ancestor) else {
            return false;
        };
        metadata.file_type().is_symlink() && ancestor.parent().and_then(Path::parent).is_some()
    })
}

impl AsRef<Path> for AbsolutePathBuf {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl std::ops::Deref for AbsolutePathBuf {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<AbsolutePathBuf> for PathBuf {
    fn from(path: AbsolutePathBuf) -> Self {
        path.into_path_buf()
    }
}

impl fmt::Display for AbsolutePathBuf {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.display().fmt(formatter)
    }
}

impl TryFrom<&Path> for AbsolutePathBuf {
    type Error = io::Error;

    fn try_from(value: &Path) -> Result<Self, Self::Error> {
        Self::from_absolute_path(value)
    }
}

impl TryFrom<PathBuf> for AbsolutePathBuf {
    type Error = io::Error;

    fn try_from(value: PathBuf) -> Result<Self, Self::Error> {
        Self::from_absolute_path(value)
    }
}

impl TryFrom<&str> for AbsolutePathBuf {
    type Error = io::Error;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::from_absolute_path(value)
    }
}

impl TryFrom<String> for AbsolutePathBuf {
    type Error = io::Error;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::from_absolute_path(value)
    }
}

impl<'de> Deserialize<'de> for AbsolutePathBuf {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let path = PathBuf::deserialize(deserializer)?;
        match AbsolutePathBufGuard::deserialization_base(&path)
            .map_err(D::Error::custom)?
            .as_deref()
        {
            Some(base) => Ok(Self::resolve_path_against_base(path, base)),
            None => Self::from_absolute_path(path).map_err(D::Error::custom),
        }
    }
}

thread_local! {
    static ABSOLUTE_PATH_BASE: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
    static ABSOLUTE_PATH_HOME: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

/// The synchronous base for deserializing absolute paths.
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
            .or_else(dirs::home_dir)
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

/// Expands a leading home-relative spelling using the synchronous home override or host home.
/// Other path text is retained without looking up the current directory or filesystem.
#[must_use]
pub fn expand_home_directory(path: &Path) -> PathBuf {
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

fn normalize_path_for_platform(path: &Path) -> Cow<'_, Path> {
    if cfg!(windows)
        && let Some(path) = path.to_str()
        && let Some(normalized) = normalize_windows_device_path(path)
    {
        return Cow::Owned(PathBuf::from(normalized));
    }

    Cow::Borrowed(path)
}

/// Drops the device-namespace prefix of a Windows drive or UNC path: Codex's
/// `normalize_windows_device_path`.
#[must_use]
pub fn normalize_windows_device_path(path: &str) -> Option<String> {
    if let Some(unc) = path.strip_prefix(r"\\?\UNC\") {
        return Some(format!(r"\\{unc}"));
    }
    if let Some(unc) = path.strip_prefix(r"\\.\UNC\") {
        return Some(format!(r"\\{unc}"));
    }
    if let Some(path) = path.strip_prefix(r"\\?\")
        && is_windows_drive_absolute_path(path)
    {
        return Some(path.to_owned());
    }
    if let Some(path) = path.strip_prefix(r"\\.\")
        && is_windows_drive_absolute_path(path)
    {
        return Some(path.to_owned());
    }
    None
}

fn is_windows_drive_absolute_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/')
}

// Adapted from path-absolutize 3.1.1, Copyright (c) 2018 magiclen.org (Ron Li).
// Licensed under the MIT License retained in third_party/path-absolutize.LICENSE.
fn absolutize(path: &Path) -> io::Result<PathBuf> {
    if path.is_absolute() {
        return Ok(normalize_path(path));
    }

    Ok(normalize_path(&path_with_base(
        path,
        &std::env::current_dir()?,
    )))
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }

    if normalized.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        normalized
    }
}

#[cfg(not(windows))]
fn path_with_base(path: &Path, base_path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base_path.join(path)
    }
}

#[cfg(windows)]
fn path_with_base(path: &Path, base_path: &Path) -> PathBuf {
    if path.is_absolute() || path.has_root() {
        return base_path.join(path);
    }

    let mut components = path.components();
    let Some(Component::Prefix(prefix)) = components.next() else {
        return base_path.join(path);
    };

    let mut path = PathBuf::new();
    path.push(prefix.as_os_str());

    if components.clone().next().is_none() {
        path.push(std::path::MAIN_SEPARATOR_STR);
        return path;
    }

    let skip_base_prefix = matches!(base_path.components().next(), Some(Component::Prefix(_)));
    for component in base_path
        .components()
        .skip(usize::from(skip_base_prefix))
        .chain(components)
    {
        path.push(component.as_os_str());
    }
    path
}
