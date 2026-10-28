// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
#![allow(
    missing_docs,
    reason = "Preserve source product configuration and validation records."
)]
use dirs::home_dir;
use ra_paths::AbsolutePathBuf;
use std::path::PathBuf;

/// Returns the path to the product configuration directory, which can be
/// specified by the `RA_HOME` environment variable. If not set, defaults to
/// `~/.rusty-agent`.
///
/// - If `RA_HOME` is set, the value must exist and be a directory. The
///   value will be canonicalized and this function will Err otherwise.
/// - If `RA_HOME` is not set, this function does not verify that the
///   directory exists.
pub fn find_ra_home() -> std::io::Result<AbsolutePathBuf> {
    let codex_home_env = std::env::var("RA_HOME").ok().filter(|val| !val.is_empty());
    find_ra_home_from_env(codex_home_env.as_deref())
}

#[allow(
    clippy::if_not_else,
    reason = "Preserve source home selection and error branches."
)]
#[allow(
    clippy::single_match_else,
    reason = "Preserve source home selection and error branches."
)]
fn find_ra_home_from_env(codex_home_env: Option<&str>) -> std::io::Result<AbsolutePathBuf> {
    // Honor the `RA_HOME` environment variable when it is set to allow users
    // (and tests) to override the default location.
    match codex_home_env {
        Some(val) => {
            let path = PathBuf::from(val);
            let metadata = std::fs::metadata(&path).map_err(|err| match err.kind() {
                std::io::ErrorKind::NotFound => std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("RA_HOME points to {val:?}, but that path does not exist"),
                ),
                _ => std::io::Error::new(
                    err.kind(),
                    format!("failed to read RA_HOME {val:?}: {err}"),
                ),
            })?;

            if !metadata.is_dir() {
                Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("RA_HOME points to {val:?}, but that path is not a directory"),
                ))
            } else {
                let canonical = path.canonicalize().map_err(|err| {
                    std::io::Error::new(
                        err.kind(),
                        format!("failed to canonicalize RA_HOME {val:?}: {err}"),
                    )
                })?;
                AbsolutePathBuf::from_absolute_path(canonical)
            }
        }
        None => {
            let mut p = home_dir().ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "Could not find home directory",
                )
            })?;
            p.push(".rusty-agent");
            AbsolutePathBuf::from_absolute_path(p)
        }
    }
}

/// Test-only home selection without mutating the process environment.
#[cfg(feature = "test-api")]
#[doc(hidden)]
pub fn test_find_ra_home_from_env(value: Option<&str>) -> std::io::Result<AbsolutePathBuf> {
    find_ra_home_from_env(value)
}
