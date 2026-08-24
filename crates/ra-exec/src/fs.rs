//! Capability-scoped file access used by tools that operate on workspace paths.
//!
//! A root is opened once and every later lookup runs from that directory handle. Resolution is
//! cap-std's: components are walked from the handle, a symbolic link is followed only as far as it
//! stays below the root, and one that leaves is refused instead of opened. Checking a pathname
//! and then reopening it therefore cannot turn a workspace read or write into an access outside
//! the workspace.

use std::{
    fmt,
    fs::File,
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
};

use cap_std::{ambient_authority, fs::Dir};

/// A directory capability that can open paths below one workspace root.
///
/// Links are followed, not refused. A workspace whose `src` is a symbolic link is an ordinary
/// workspace, and a reader that cannot enter it is broken rather than safe. What makes following
/// them sound here is that the resolver walks from the root's descriptor and refuses the step that
/// would leave it — a decision taken during the open, not from a pathname inspected beforehand.
///
/// Two link shapes are still refused, both by cap-std rather than by this type:
///
/// - an **absolute** target, even one pointing back inside the root — an absolute path means
///   nothing relative to a directory handle, so there is no sound way to admit it;
/// - a **relative** target that climbs above the root, whatever it points at afterwards.
pub struct RootedFileSystem {
    root: Dir,
}

impl fmt::Debug for RootedFileSystem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RootedFileSystem")
            .finish_non_exhaustive()
    }
}

/// Why a rooted filesystem operation did not succeed.
///
/// The two variants are two different answers, not two spellings of one: `OutsideRoot` is the
/// boundary refusing, and `Io` is the filesystem refusing a request that stayed inside it. A
/// caller that collapses them tells the model "no such file" for a boundary violation, or the
/// reverse.
#[non_exhaustive]
#[derive(Debug)]
pub enum RootedOpenError {
    /// The requested path attempted to leave the capability root.
    OutsideRoot,
    /// The underlying filesystem rejected a path that stayed inside the root.
    Io(io::Error),
}

impl fmt::Display for RootedOpenError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutsideRoot => formatter.write_str("the path leaves the root"),
            Self::Io(error) => write!(formatter, "the path could not be accessed: {error}"),
        }
    }
}

impl std::error::Error for RootedOpenError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::OutsideRoot => None,
            Self::Io(error) => Some(error),
        }
    }
}

impl RootedFileSystem {
    /// Opens `root` once, turning it into the capability used by future relative operations.
    pub fn open(root: impl AsRef<Path>) -> io::Result<Self> {
        Dir::open_ambient_dir(root, ambient_authority()).map(|root| Self { root })
    }

    /// Opens one path below the root for reading.
    ///
    /// The returned descriptor, rather than the input pathname, is what a caller must inspect and
    /// read. Holding that descriptor across metadata and reads is what closes the classic
    /// `canonicalize`-then-`open` race.
    pub fn open_read(&self, path: &Path) -> Result<File, RootedOpenError> {
        let relative = relative_path(path)?;
        if relative.as_os_str().is_empty() {
            return self
                .root
                .try_clone()
                .map(Dir::into_std_file)
                .map_err(RootedOpenError::Io);
        }
        self.root
            .open(&relative)
            .map(cap_std::fs::File::into_std)
            .map_err(classify)
    }

    /// Reads up to `max_bytes` of UTF-8 content from a file below the root.
    ///
    /// Returns `(content, is_truncated)`. If truncation cuts across a multibyte UTF-8 codepoint,
    /// the returned content is cleanly sliced at the last complete character boundary.
    pub fn read_to_string(
        &self,
        path: &Path,
        max_bytes: usize,
    ) -> Result<(String, bool), RootedOpenError> {
        let mut file = self.open_read(path)?;
        let mut buffer = Vec::new();
        let take_limit = (max_bytes as u64).saturating_add(1);
        let mut take = (&mut file).take(take_limit);
        take.read_to_end(&mut buffer).map_err(RootedOpenError::Io)?;

        let is_truncated = buffer.len() > max_bytes;
        if is_truncated {
            buffer.truncate(max_bytes);
        }

        let content = match std::str::from_utf8(&buffer) {
            Ok(s) => s.to_owned(),
            Err(e) => {
                let valid_up_to = e.valid_up_to();
                if is_truncated && valid_up_to > 0 && e.error_len().is_none() {
                    std::str::from_utf8(&buffer[..valid_up_to])
                        .unwrap_or("")
                        .to_owned()
                } else {
                    String::from_utf8_lossy(&buffer).into_owned()
                }
            }
        };

        Ok((content, is_truncated))
    }

    /// Writes data to a file below the root, creating parent directories as needed.
    pub fn write_file(&self, path: &Path, content: &[u8]) -> Result<(), RootedOpenError> {
        let relative = relative_path(path)?;
        let mut file = match self.root.create(&relative) {
            Ok(file) => file.into_std(),
            Err(err) => {
                if err.kind() == io::ErrorKind::NotFound {
                    if let Some(parent) = relative.parent()
                        && !parent.as_os_str().is_empty()
                    {
                        self.create_dir_all(parent)?;
                        self.root
                            .create(&relative)
                            .map(cap_std::fs::File::into_std)
                            .map_err(classify)?
                    } else {
                        return Err(classify(err));
                    }
                } else {
                    return Err(classify(err));
                }
            }
        };
        file.write_all(content).map_err(RootedOpenError::Io)
    }

    /// Creates directories recursively below the root.
    pub fn create_dir_all(&self, path: &Path) -> Result<(), RootedOpenError> {
        let relative = relative_path(path)?;
        if relative.as_os_str().is_empty() {
            return Ok(());
        }
        self.root.create_dir_all(&relative).map_err(classify)
    }

    /// Removes a file below the root.
    pub fn remove_file(&self, path: &Path) -> Result<(), RootedOpenError> {
        let relative = relative_path(path)?;
        self.root.remove_file(&relative).map_err(classify)
    }

    /// Renames a file below the root without reopening either path by ambient name.
    pub fn rename(&self, from: &Path, to: &Path) -> Result<(), RootedOpenError> {
        let from = relative_path(from)?;
        let to = relative_path(to)?;
        if let Some(parent) = to.parent()
            && !parent.as_os_str().is_empty()
        {
            self.create_dir_all(parent)?;
        }
        self.root.rename(&from, &self.root, &to).map_err(classify)
    }

    /// Checks if a path exists below the root.
    #[must_use]
    pub fn exists(&self, path: &Path) -> bool {
        let Ok(relative) = relative_path(path) else {
            return false;
        };
        if relative.as_os_str().is_empty() {
            return true;
        }
        self.root.exists(&relative)
    }
}

/// Rejects what can be decided without touching the disk, and returns the rest.
///
/// A path that names its way out — absolute, or climbing with `..` — is refused here rather than
/// handed to the resolver, so the refusal costs no syscall and reveals nothing about what is or is
/// not on the other side of the boundary.
fn relative_path(path: &Path) -> Result<PathBuf, RootedOpenError> {
    let mut relative = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(component) => relative.push(component),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(RootedOpenError::OutsideRoot);
            }
        }
    }
    Ok(relative)
}

/// Labels a refusal cap-std has already made.
///
/// cap-std uses synthetic `PermissionDenied` errors without a raw OS error code to represent
/// boundary refusals (e.g. attempting to traverse out of the rooted descriptor), whereas real
/// OS-level permission errors (such as `EACCES` / `EPERM`) carry a concrete `raw_os_error`.
fn classify(error: io::Error) -> RootedOpenError {
    if error.kind() == io::ErrorKind::PermissionDenied && error.raw_os_error().is_none() {
        RootedOpenError::OutsideRoot
    } else {
        RootedOpenError::Io(error)
    }
}

/// Derives a resource identity for a workspace filesystem.
pub fn workspace_resource_id(
    identifier: impl Into<std::borrow::Cow<'static, str>>,
) -> Result<ra_core::tool::ResourceId, ra_core::error::Error> {
    ra_core::tool::ResourceId::workspace(identifier)
}
