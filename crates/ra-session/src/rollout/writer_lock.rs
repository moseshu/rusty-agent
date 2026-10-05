//! Stable thread ownership, ported from Codex's `rollout/src/writer_lock.rs`.
//!
//! Coordination serializes lock-file creation, stale cleanup and removal so contenders cannot
//! lock different inodes for the same thread. Ownership outlives any rollout file reopened after
//! I/O failure. Session ids replace Codex's UUID thread ids; the rollout filename validator applies
//! to both namespaces. Unix uses rustix rather than `File::lock` to preserve the crate's MSRV;
//! unsupported platforms are refused as the rollout writer already refuses them.

use std::{
    fs::{self, File, OpenOptions},
    io,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use ra_core::{
    error::{Error, Result, SessionErrorKind},
    session::SessionId,
};

use super::writer::session_rollout_path;

const WRITER_LOCK_DIR: &str = "thread-writer-locks";
const COORDINATION_LOCK_FILE: &str = ".coordination.lock";

#[derive(Debug)]
pub(crate) struct WriterLockCoordinator {
    directory: PathBuf,
    cleanup_attempted: AtomicBool,
}

#[derive(Debug)]
pub(crate) struct WriterLockGuard {
    coordinator: Arc<WriterLockCoordinator>,
    path: PathBuf,
    file: Option<File>,
}

impl WriterLockCoordinator {
    pub(crate) fn new(directory: &Path) -> Self {
        Self {
            directory: directory.join(WRITER_LOCK_DIR),
            cleanup_attempted: AtomicBool::new(false),
        }
    }

    pub(crate) fn acquire(
        self: &Arc<Self>,
        session_id: &SessionId,
    ) -> Result<Arc<WriterLockGuard>> {
        session_rollout_path(&self.directory, session_id)?;
        self.acquire_inner(session_id)
            .map(Arc::new)
            .map_err(|error| {
                Error::session(SessionErrorKind::Io, error.to_string()).with_source(error)
            })
    }

    fn acquire_inner(self: &Arc<Self>, session_id: &SessionId) -> io::Result<WriterLockGuard> {
        let _coordination = self.lock_coordination()?;
        if !self.cleanup_attempted.swap(true, Ordering::Relaxed)
            && let Err(error) = self.remove_stale_thread_locks()
        {
            tracing::warn!(%error, "failed to clean up stale thread writer locks");
        }
        let path = self.directory.join(format!("{}.lock", session_id.as_str()));
        let file = open_lock(&path)?;
        lock(&file, true).map_err(|error| {
            if error.kind() == io::ErrorKind::WouldBlock {
                io::Error::new(
                    io::ErrorKind::WouldBlock,
                    format!("another writer already holds thread `{session_id}`"),
                )
            } else {
                io::Error::other(format!("failed to lock thread `{session_id}`: {error}"))
            }
        })?;
        Ok(WriterLockGuard {
            coordinator: Arc::clone(self),
            path,
            file: Some(file),
        })
    }

    fn lock_coordination(&self) -> io::Result<File> {
        fs::create_dir_all(&self.directory)?;
        let file = open_lock(&self.directory.join(COORDINATION_LOCK_FILE))?;
        lock(&file, false)?;
        Ok(file)
    }

    fn remove_stale_thread_locks(&self) -> io::Result<()> {
        for entry in fs::read_dir(&self.directory)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name == COORDINATION_LOCK_FILE {
                continue;
            }
            let Some(id) = name.strip_suffix(".lock") else {
                continue;
            };
            if session_rollout_path(&self.directory, &SessionId::new(id)).is_err() {
                continue;
            }
            let path = entry.path();
            let file = match OpenOptions::new().read(true).write(true).open(&path) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => {
                    tracing::warn!(%error, path = %path.display(), "failed to inspect a thread writer lock");
                    continue;
                }
            };
            match lock(&file, true) {
                Ok(()) => {
                    drop(file);
                    if let Err(error) = fs::remove_file(&path)
                        && error.kind() != io::ErrorKind::NotFound
                    {
                        tracing::warn!(%error, path = %path.display(), "failed to remove a stale thread writer lock");
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => {
                    tracing::warn!(%error, path = %path.display(), "failed to inspect a thread writer lock");
                }
            }
        }
        Ok(())
    }
}

impl Drop for WriterLockGuard {
    fn drop(&mut self) {
        let _coordination = match self.coordinator.lock_coordination() {
            Ok(file) => file,
            Err(error) => {
                tracing::warn!(%error, "failed to coordinate thread writer lock cleanup");
                return;
            }
        };
        drop(self.file.take());
        if let Err(error) = fs::remove_file(&self.path)
            && error.kind() != io::ErrorKind::NotFound
        {
            tracing::warn!(%error, path = %self.path.display(), "failed to remove a thread writer lock");
        }
    }
}

fn open_lock(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
}

#[cfg(unix)]
fn lock(file: &File, nonblocking: bool) -> io::Result<()> {
    use rustix::fs::{FlockOperation, flock};
    flock(
        file,
        if nonblocking {
            FlockOperation::NonBlockingLockExclusive
        } else {
            FlockOperation::LockExclusive
        },
    )
    .map_err(io::Error::from)
}

#[cfg(not(unix))]
fn lock(_file: &File, _nonblocking: bool) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "thread writing is unsupported: no platform lock is available",
    ))
}
