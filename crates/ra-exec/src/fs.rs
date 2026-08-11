//! Capability-scoped file access used by tools that operate on workspace paths.
//!
//! A root is opened once and every later lookup runs from that directory handle.  Resolution is
//! cap-std's: components are walked from the handle, a symbolic link is followed only as far as it
//! stays below the root, and one that leaves is refused instead of opened.  Checking a pathname
//! and then reopening it therefore cannot turn a workspace read into a read outside the workspace.

use std::{
    fmt,
    fs::File,
    io,
    path::{Component, Path, PathBuf},
};

use cap_std::{ambient_authority, fs::Dir};

/// A directory capability that can open paths below one workspace root.
///
/// Links are followed, not refused.  A workspace whose `src` is a symbolic link is an ordinary
/// workspace, and a reader that cannot enter it is broken rather than safe.  What makes following
/// them sound here is that the resolver walks from the root's descriptor and refuses the step that
/// would leave it — a decision taken during the open, not from a pathname inspected beforehand.
///
/// Two link shapes are still refused, both by cap-std rather than by this type:
///
/// * an **absolute** target, even one pointing back inside the root — an absolute path means
///   nothing relative to a directory handle, so there is no sound way to admit it;
/// * a **relative** target that climbs above the root, whatever it points at afterwards.
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

/// Why a rooted open did not produce a file.
///
/// The two variants are two different answers, not two spellings of one: `OutsideRoot` is the
/// boundary refusing, and `Io` is the filesystem refusing a request that stayed inside it.  A
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
            Self::Io(error) => write!(formatter, "the path could not be opened: {error}"),
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
    /// Opens `root` once, turning it into the capability used by future relative opens.
    pub fn open(root: impl AsRef<Path>) -> io::Result<Self> {
        Dir::open_ambient_dir(root, ambient_authority()).map(|root| Self { root })
    }

    /// Opens one path below the root for reading.
    ///
    /// The returned descriptor, rather than the input pathname, is what a caller must inspect and
    /// read.  Holding that descriptor across metadata and reads is what closes the classic
    /// `canonicalize`-then-`open` race.
    pub fn open_read(&self, path: &Path) -> Result<File, RootedOpenError> {
        let relative = relative_path(path)?;
        if relative.as_os_str().is_empty() {
            // The root itself. Handing back its descriptor lets the caller answer "that is not a
            // regular file" from the same handle every other answer comes from.
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
/// This never decides whether an open is allowed — that was decided inside the open, from the
/// directory descriptor, before this runs.  It only picks which sentence describes the refusal, so
/// the worst a misreading can do is describe one refusal in the other's words.
///
/// cap-std reports an escape as a *synthesized* `PermissionDenied` carrying no OS error number,
/// while a directory the process genuinely may not enter arrives with `EACCES` attached.  That
/// difference, rather than the message text, is what is read here.
fn classify(error: io::Error) -> RootedOpenError {
    if error.kind() == io::ErrorKind::PermissionDenied && error.raw_os_error().is_none() {
        RootedOpenError::OutsideRoot
    } else {
        RootedOpenError::Io(error)
    }
}
