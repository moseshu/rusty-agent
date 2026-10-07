//! How a configured path is made absolute: Codex's `AbsolutePathBuf::from_absolute_path`
//! (`utils/absolute-path/src/lib.rs` and `absolutize.rs`).
//!
//! A leading `~` is the home directory. On Windows a `\\?\` or `\\.\` prefix on a drive or UNC path
//! is dropped. A relative path is resolved against the current directory, and the result is
//! normalized lexically: `.` is dropped, `..` removes the component before it and stops at the
//! root. The file system is not consulted, so the path need not exist and symbolic links are kept.
//!
//! Codex can also take the home directory from a thread-local override, which only its config
//! loading sets; here it is always the user's home directory.

use std::{
    borrow::Cow,
    io,
    path::{Component, Path, PathBuf},
};

/// `path` made absolute and normalized. Reads the current directory only when `path` is relative.
pub(crate) fn from_absolute_path(path: &Path) -> io::Result<PathBuf> {
    let expanded = maybe_expand_home_directory(path);
    let expanded = normalize_path_for_platform(&expanded);
    absolutize(&expanded)
}

fn maybe_expand_home_directory(path: &Path) -> PathBuf {
    if let Some(path_str) = path.to_str()
        && let Some(rest) = path_str.strip_prefix('~')
        && let Some(home) = std::env::home_dir()
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
fn normalize_windows_device_path(path: &str) -> Option<String> {
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
