//! A scratch directory owned by one run, and the rule for when it may be removed.
//!
//! # Why removal is a decision, not a drop
//!
//! The obvious implementation deletes the directory when the owning value goes out of scope. That
//! is wrong here in three separate situations, and each of them is ordinary rather than exotic:
//!
//! - a **backgrounded command** outlives the tool call that started it, and keeps writing into the
//!   directory after every value the call held has dropped;
//! - a **paused run** drops its values and comes back later expecting the same path, because the
//!   half-written archive it left there is the reason it is resuming;
//! - an **abnormal exit** runs no destructors at all, so anything that depends on one to clean up
//!   leaks exactly when leaking is least visible.
//!
//! So a [`RunTempDir`] is removed when someone asks and nobody is still using it. Users are counted
//! with [`TempDirUse`] handles: a session takes one when it starts and releases it when its process
//! is reaped, and [`RunTempDir::cleanup`] refuses while any are outstanding rather than pulling the
//! floor out from under a live process. The abnormal-exit case is then a feature of the same
//! design: nothing was deleted, the directory is still there, and a host recovery authority can
//! reclaim it later. A supervisor that cannot confirm descendant exit marks the directory for
//! recovery before releasing its handle. Ordinary cleanup cannot clear that mark.

use std::{
    fmt, fs, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, Weak},
};

/// The variable a spawned process reads to find its run's scratch directory.
pub const TMPDIR_ENV_VAR: &str = "RUSTY_AGENT_TMPDIR";

/// Prefix of the directory name created under the system temporary directory.
const TMPDIR_PREFIX: &str = "rusty-agent-";

/// Why a scratch directory could not be created or removed.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum TempDirError {
    /// The run key was empty or contained a path separator.
    #[error("run key `{key}` is not usable as a directory name: {reason}")]
    InvalidKey {
        /// The rejected key.
        key: String,
        /// What was wrong with it.
        reason: &'static str,
    },
    /// Creating the directory failed.
    #[error("could not create scratch directory at `{path}`")]
    Create {
        /// Where it was going.
        path: PathBuf,
        /// The underlying failure.
        #[source]
        source: io::Error,
    },
    /// Removal was asked for while processes were still using the directory.
    #[error("scratch directory `{path}` still has {users} user(s)")]
    InUse {
        /// The directory that was left alone.
        path: PathBuf,
        /// How many handles were outstanding.
        users: usize,
    },
    /// A process may still use the directory after its supervisor finished.
    #[error("scratch directory `{path}` requires host recovery: descendant exit was not confirmed")]
    RecoveryRequired {
        /// The directory retained for recovery.
        path: PathBuf,
    },
    /// Removing the directory failed.
    #[error("could not remove scratch directory at `{path}`")]
    Remove {
        /// What was left behind.
        path: PathBuf,
        /// The underlying failure.
        #[source]
        source: io::Error,
    },
}

/// A scratch directory belonging to one run.
///
/// Created under the system temporary directory with a name derived from the run's key, so active handles can share a directory. Existing directories without a live ownership record
/// are refused; cross-process recovery requires a separate host recovery authority.
/// The key is opaque here: hosts pass their run identifier, and this type only checks that it can
/// be a single path component.
#[derive(Debug)]
pub struct RunTempDir {
    path: PathBuf,
    parent: PathBuf,
    state: Arc<Mutex<DirectoryState>>,
}

#[derive(Debug)]
struct DirectoryState {
    users: usize,
    removed: bool,
    recovery_required: bool,
    identity: std::fs::Metadata,
    directory: cap_std::fs::Dir,
}

/// Whether a removal honours the recovery mark or has been authorised past it.
#[derive(Debug, Clone, Copy)]
enum RecoveryGate {
    Enforce,
    Bypass,
}

type DirectoryRegistry = std::collections::HashMap<PathBuf, Weak<Mutex<DirectoryState>>>;
static DIRECTORIES: OnceLock<Mutex<DirectoryRegistry>> = OnceLock::new();

fn directory_identity_matches(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        a.dev() == b.dev() && a.ino() == b.ino()
    }
    #[cfg(not(unix))]
    {
        a.created()
            .ok()
            .zip(b.created().ok())
            .is_some_and(|(a, b)| a == b)
    }
}

impl RunTempDir {
    /// Creates, or shares an active handle to, the scratch directory for `key` under the system temporary directory.
    ///
    /// # Errors
    ///
    /// Returns [`TempDirError::InvalidKey`] when the key cannot be a directory name, and
    /// [`TempDirError::Create`] when the directory cannot be made.
    pub fn open(key: &str) -> Result<Self, TempDirError> {
        Self::open_in(std::env::temp_dir(), key)
    }

    /// Creates, or shares an active handle to, the scratch directory for `key` under an explicit parent.
    ///
    /// The parent belongs to whoever passed it and is never created or removed by this
    /// type — only the one directory below it is.
    ///
    /// # Errors
    ///
    /// As [`Self::open`]. Existing paths without a live ownership record are rejected.
    /// Recovery after process exit must be authorized separately by the host.
    pub fn open_in(parent: impl Into<PathBuf>, key: &str) -> Result<Self, TempDirError> {
        let parent = parent.into();
        let name = directory_name(key)?;
        let create_error = |source| TempDirError::Create {
            path: parent.join(&name),
            source,
        };
        let parent = fs::canonicalize(&parent).map_err(create_error)?;
        let path = parent.join(name);
        let mut registry = DIRECTORIES
            .get_or_init(|| Mutex::new(DirectoryRegistry::new()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        registry.retain(|_, state| state.strong_count() > 0);
        if let Some(state) = registry.get(&path).and_then(Weak::upgrade) {
            // A state whose directory has already been cleaned is not shareable. Its `removed` flag
            // refuses every later handle, so returning it would answer a request for a usable
            // directory with an `Ok` naming one that is gone. Fall through instead and make a new
            // directory, replacing the registry entry; values still holding the old state keep
            // refusing handles, which is what they are for.
            let shareable = !state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .removed;
            if shareable {
                return Ok(Self {
                    path,
                    parent,
                    state,
                });
            }
        }
        // Existing paths require a separate recovery authority. A name alone proves no ownership.
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(&path)
            .map_err(|source| TempDirError::Create {
                path: path.clone(),
                source,
            })?;
        let identity = fs::symlink_metadata(&path).map_err(|source| TempDirError::Create {
            path: path.clone(),
            source,
        })?;
        let directory = cap_std::fs::Dir::open_ambient_dir(&path, cap_std::ambient_authority())
            .map_err(|source| TempDirError::Create {
                path: path.clone(),
                source,
            })?;
        let state = Arc::new(Mutex::new(DirectoryState {
            users: 0,
            removed: false,
            recovery_required: false,
            identity,
            directory,
        }));
        registry.insert(path.clone(), Arc::downgrade(&state));
        Ok(Self {
            path,
            parent,
            state,
        })
    }

    /// The directory's path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Registers one more user and hands back the handle that releases it.
    ///
    /// A caller that starts a process into this directory takes one of these and keeps it for as
    /// long as the process can still write — which, for a backgrounded command, is well past the
    /// end of the call that started it.
    ///
    /// # Errors
    ///
    /// Returns an error after cleanup or when the directory identity has changed.
    pub fn use_handle(&self) -> Result<TempDirUse, TempDirError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.removed {
            return Err(TempDirError::Remove {
                path: self.path.clone(),
                source: io::Error::new(
                    io::ErrorKind::NotFound,
                    "scratch directory has been cleaned",
                ),
            });
        }
        let metadata = fs::symlink_metadata(&self.path).map_err(|source| TempDirError::Remove {
            path: self.path.clone(),
            source,
        })?;
        if !metadata.is_dir() || !directory_identity_matches(&metadata, &state.identity) {
            return Err(TempDirError::Remove {
                path: self.path.clone(),
                source: io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "scratch directory identity changed",
                ),
            });
        }
        state.users += 1;
        Ok(TempDirUse {
            state: Arc::clone(&self.state),
        })
    }

    /// How many users are currently registered.
    #[must_use]
    pub fn users(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .users
    }

    /// Whether this directory is held back for a host to verify before it can be removed.
    ///
    /// Set when the executor could not establish that everything it was tracking had exited. It is
    /// sticky — see the private `TempDirUse::require_recovery` — so a host reports and acts on
    /// it rather than waiting for it to clear.
    #[must_use]
    pub fn recovery_required(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .recovery_required
    }

    /// Removes the directory and everything in it, if nobody is using it.
    ///
    /// Idempotent: removing a directory that is already gone succeeds, because the caller's
    /// intent — "this should not exist" — is satisfied either way, and a closeout path that has to
    /// distinguish "I removed it" from "it was already removed" would report a failure for the
    /// normal case of running twice.
    ///
    /// # Errors
    ///
    /// Returns [`TempDirError::InUse`] when handles are outstanding, leaving the directory intact,
    /// [`TempDirError::RecoveryRequired`] when descendant exit was not confirmed,
    /// and [`TempDirError::Remove`] when removal itself fails. A directory that turns out not to be
    /// one — a symbolic link left in its place, say — is reported rather than followed, because
    /// following it would delete whatever it points at.
    pub fn cleanup(&self) -> Result<(), TempDirError> {
        self.remove(RecoveryGate::Enforce)
    }

    /// Removes the directory after a host has established for itself that nothing is still using it.
    ///
    /// The one gate this passes is the recovery mark; every other check still runs, including the
    /// outstanding-handle count and the directory's identity. That is the point of the method
    /// existing at all: without it a host that had done the verification would have to remove the
    /// path with plain filesystem calls, losing both the identity check and the removal through an
    /// already-open directory handle — so the case that most needs care would be the one running on
    /// the least careful path.
    ///
    /// **Verifying is the caller's job and this type cannot check the work.** What it means is a
    /// host question — that a process group is gone, that a container has been torn down, that a
    /// machine has been rebooted since. Calling this without having answered it removes a directory
    /// that something may still be writing into.
    ///
    /// # Errors
    ///
    /// As [`Self::cleanup`], less [`TempDirError::RecoveryRequired`].
    pub fn cleanup_after_recovery(&self) -> Result<(), TempDirError> {
        self.remove(RecoveryGate::Bypass)
    }

    fn remove(&self, gate: RecoveryGate) -> Result<(), TempDirError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.removed {
            return Ok(());
        }
        if state.recovery_required && matches!(gate, RecoveryGate::Enforce) {
            return Err(TempDirError::RecoveryRequired {
                path: self.path.clone(),
            });
        }
        // Outstanding handles are checked either way. They name processes this executor is still
        // tracking, and no amount of host verification about *untracked* descendants says anything
        // about those.
        let users = state.users;
        if users > 0 {
            return Err(TempDirError::InUse {
                path: self.path.clone(),
                users,
            });
        }

        let metadata = match fs::symlink_metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                state.removed = true;
                return Ok(());
            }
            Err(source) => {
                return Err(TempDirError::Remove {
                    path: self.path.clone(),
                    source,
                });
            }
        };

        // Refuse anything that is not a plain directory. `remove_dir_all` on a symbolic link
        // removes the link rather than its target on current platforms, but the check is cheap and
        // the failure it guards against — deleting a directory elsewhere on the disk because
        // something replaced ours — is not one to leave to platform behaviour.
        if !metadata.is_dir() || !directory_identity_matches(&metadata, &state.identity) {
            return Err(TempDirError::Remove {
                path: self.path.clone(),
                source: io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "scratch path is not a directory",
                ),
            });
        }

        state
            .directory
            .try_clone()
            .and_then(cap_std::fs::Dir::remove_open_dir_all)
            .map_err(|source| TempDirError::Remove {
                path: self.path.clone(),
                source,
            })?;
        state.removed = true;
        Ok(())
    }

    /// The directory this one was created under, which is the caller's and is never touched.
    #[must_use]
    pub fn parent(&self) -> &Path {
        &self.parent
    }
}

impl fmt::Display for RunTempDir {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.path.display())
    }
}

/// One registered user of a [`RunTempDir`], released when dropped.
///
/// Cheap to clone-by-taking-another: each handle is one count, and the directory cannot be removed
/// while any exist.
#[derive(Debug)]
pub struct TempDirUse {
    state: Arc<Mutex<DirectoryState>>,
}

impl TempDirUse {
    /// Retains the directory when the executor cannot prove all tracked users have exited.
    /// This is sticky: a later PID probe cannot safely distinguish a reused process identifier.
    pub(crate) fn require_recovery(&self) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .recovery_required = true;
    }
}

impl Drop for TempDirUse {
    fn drop(&mut self) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .users -= 1;
    }
}

/// Builds the single path component a run's directory is named with.
fn directory_name(key: &str) -> Result<String, TempDirError> {
    let invalid = |reason| TempDirError::InvalidKey {
        key: key.to_string(),
        reason,
    };
    if key.is_empty() {
        return Err(invalid("it is empty"));
    }
    if key.contains(['/', '\\']) {
        return Err(invalid("it contains a path separator"));
    }
    if key == "." || key == ".." {
        return Err(invalid("it names a relative directory"));
    }
    if key.contains('\0') {
        return Err(invalid("it contains a null byte"));
    }
    Ok(format!("{TMPDIR_PREFIX}{key}"))
}
