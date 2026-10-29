// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Permission-profile path grammar. Path facts and resolution are owned by
//! the shared configuration context; symbolic paths preserve native spelling.

use std::io;

use crate::ConfigPathContext;
use ra_paths::LegacyAppPathString;
use ra_paths::PathConvention;
use ra_paths::PathUri;
use ra_paths::normalize_windows_device_path;

#[allow(
    clippy::unnecessary_wraps,
    reason = "Preserve the source fallible path-grammar interface."
)]
pub(super) fn contains_glob(path: &str, context: &ConfigPathContext) -> io::Result<bool> {
    Ok(contains_glob_chars_for_platform(
        path,
        context.convention() == PathConvention::Windows,
    ))
}

pub(super) fn absolute_path(path: &str, context: &ConfigPathContext) -> io::Result<PathUri> {
    let convention = context.convention();
    if convention.home_relative_suffix(path).is_none()
        && LegacyAppPathString::from_string(path)
            .to_path_uri(convention)
            .is_err()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("filesystem path `{path}` must be absolute, use `~/...`, or start with `:`"),
        ));
    }
    context
        .resolve_path(path)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))
}

pub(crate) fn relative_subpath(subpath: &str, context: &ConfigPathContext) -> io::Result<String> {
    let convention = context.convention();
    let mut components = convention.path_segments(subpath);
    let first = components.next();
    let has_drive_prefix = convention == PathConvention::Windows
        && subpath
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphabetic)
        && subpath.as_bytes().get(1) == Some(&b':');
    if !subpath.is_empty()
        && !matches!(first, Some("" | "." | ".."))
        && !has_drive_prefix
        && !components.any(|component| component == "..")
    {
        // Preserve spelling after the same validation as native components:
        // interior `.` and repeated separators stay in symbolic rules.
        return Ok(subpath.to_string());
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!(
            "filesystem subpath `{subpath}` must be a descendant path without `.` or `..` components"
        ),
    ))
}

pub(super) fn contains_glob_chars_for_platform(path: &str, is_windows: bool) -> bool {
    let normalized_windows_path = if is_windows {
        normalize_windows_device_path(path)
    } else {
        None
    };
    let path = normalized_windows_path.as_deref().unwrap_or(path);
    path.chars().any(|ch| matches!(ch, '*' | '?' | '[' | ']'))
}
