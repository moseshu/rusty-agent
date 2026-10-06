//! The file work of archiving, unarchiving and deleting a rollout directory's threads, ported from
//! Codex's local store (`thread-store/src/local/{archive_thread,unarchive_thread,delete_thread}.rs`).
//!
//! Every operation here is synchronous, as Codex's file moves are, and runs to completion once
//! started: the directory runs it on a blocking task that also owns the writer locks it was given,
//! so dropping the caller's future can neither stop it between two moves nor let go of a thread
//! while its files are half moved. A move never replaces a rollout already at its destination.
//!
//! When the directory keeps an index of its threads, it follows each move and deletion in the same
//! task, as Codex's local store updates its state database: a move the index cannot follow is
//! undone and reported, and a deletion it cannot follow is reported once the files are gone.

use std::{
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

use ra_core::{
    error::{Error, Result, SessionErrorKind},
    session::SessionId,
};

use crate::store::{index::ThreadIndex, thread_exists};

use super::{
    session_index,
    writer::{session_rollout_path, sidecar_path_for},
    writer_lock::WriterLockGuard,
};

/// The directory archived rollouts move to: Codex's `ARCHIVED_SESSIONS_SUBDIR`.
pub(crate) const ARCHIVED_SESSIONS_SUBDIR: &str = "archived_sessions";

/// Where a directory's rollouts are, active and archived.
#[derive(Debug, Clone)]
pub(crate) struct ThreadFiles {
    dir: PathBuf,
    index: Option<Arc<dyn ThreadIndex>>,
}

impl ThreadFiles {
    pub(crate) fn new(dir: &Path, index: Option<Arc<dyn ThreadIndex>>) -> Self {
        Self {
            dir: dir.to_path_buf(),
            index,
        }
    }

    pub(crate) fn archived_dir(&self) -> PathBuf {
        self.dir.join(ARCHIVED_SESSIONS_SUBDIR)
    }

    fn active(&self, session_id: &SessionId) -> Result<PathBuf> {
        session_rollout_path(&self.dir, session_id)
    }

    fn archived(&self, session_id: &SessionId) -> Result<PathBuf> {
        session_rollout_path(&self.archived_dir(), session_id)
    }

    /// Refuses a session the directory already holds a rollout of, active or archived: the
    /// conflict check both ways of creating a thread share.
    pub(crate) fn ensure_new(&self, session_id: &SessionId) -> Result<()> {
        for path in [self.active(session_id)?, self.archived(session_id)?] {
            match std::fs::symlink_metadata(&path) {
                Ok(_) => return Err(thread_exists(session_id)),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(lookup_error(session_id, &error)),
            }
        }
        Ok(())
    }

    /// Archives the threads of `session_ids` in order, as Codex's `archive_threads` does once it
    /// holds every writer lock: the first must be archived, a later failure is logged and skipped.
    pub(crate) fn archive(
        &self,
        session_ids: &[SessionId],
        _ownership: Vec<Arc<WriterLockGuard>>,
    ) -> Result<Vec<SessionId>> {
        let mut archived = Vec::new();
        for session_id in session_ids {
            match self.archive_one(session_id) {
                Ok(()) => archived.push(session_id.clone()),
                Err(error) if archived.is_empty() => return Err(error),
                Err(error) => tracing::warn!(%session_id, %error, "failed to archive a thread"),
            }
        }
        Ok(archived)
    }

    fn archive_one(&self, session_id: &SessionId) -> Result<()> {
        let source = self.active(session_id)?;
        if !is_file(&source, session_id)? {
            return Err(not_found(session_id));
        }
        std::fs::create_dir_all(self.archived_dir())
            .map_err(|error| move_error("archive", session_id, &error))?;
        let destination = self.archived(session_id)?;
        move_rollout(&source, &destination).map_err(|error| {
            if error.kind() == io::ErrorKind::AlreadyExists {
                Error::caller(format!(
                    "an archived rollout of session `{session_id}` is already kept here"
                ))
            } else {
                move_error("archive", session_id, &error)
            }
        })?;
        if let Some(index) = &self.index
            && let Err(error) = index.mark_archived(session_id, &destination)
        {
            restore(&destination, &source, session_id);
            return Err(error);
        }
        Ok(())
    }

    /// Moves the archived rollout of `session_id` back and marks it modified now, as Codex's
    /// `unarchive_thread` does, returning where it is now.
    pub(crate) fn unarchive(
        &self,
        session_id: &SessionId,
        _ownership: Arc<WriterLockGuard>,
    ) -> Result<PathBuf> {
        let destination = self.active(session_id)?;
        // As Codex resolves the thread's current rollout first: an active one shadows the
        // archived copy, which is then not unarchived over it.
        let shadowed = || {
            Error::caller(format!(
                "the thread of session `{session_id}` is active; it has no archived rollout to restore"
            ))
        };
        if is_file(&destination, session_id)? {
            return Err(shadowed());
        }
        let source = self.archived(session_id)?;
        if !is_file(&source, session_id)? {
            return Err(Error::session(
                SessionErrorKind::NotFound,
                format!("no archived rollout of session `{session_id}` is kept here"),
            ));
        }
        move_rollout(&source, &destination).map_err(|error| {
            if error.kind() == io::ErrorKind::AlreadyExists {
                shadowed()
            } else {
                move_error("unarchive", session_id, &error)
            }
        })?;
        let touched = std::fs::File::options()
            .write(true)
            .open(&destination)
            .and_then(|file| file.set_modified(std::time::SystemTime::now()));
        if let Err(error) = touched {
            restore(&destination, &source, session_id);
            return Err(move_error("unarchive", session_id, &error));
        }
        if let Some(index) = &self.index
            && let Err(error) = index.mark_unarchived(session_id, &destination)
        {
            restore(&destination, &source, session_id);
            return Err(error);
        }
        Ok(destination)
    }

    /// Deletes the threads of `session_ids` in order, as Codex's `delete_threads` does once it
    /// holds every writer lock. With `missing_is_deleted`, a thread already gone counts as deleted;
    /// without, it is reported, as Codex's `delete_thread` reports it.
    pub(crate) fn delete(
        &self,
        session_ids: &[SessionId],
        missing_is_deleted: bool,
        _ownership: Vec<Arc<WriterLockGuard>>,
    ) -> Result<()> {
        for session_id in session_ids {
            match self.delete_one(session_id) {
                Ok(()) => {}
                Err(error) if missing_is_deleted && is_not_found(&error) => {}
                Err(error) => return Err(error),
            }
        }
        // As Codex's: the rows go last, once every rollout is gone.
        match &self.index {
            Some(index) => index.delete_threads(session_ids),
            None => Ok(()),
        }
    }

    /// Deletes the active and archived rollouts and their sidecars, then the thread's names, as
    /// Codex's does whether or not a rollout was found.
    fn delete_one(&self, session_id: &SessionId) -> Result<()> {
        let mut found = false;
        for path in [self.active(session_id)?, self.archived(session_id)?] {
            found |= remove_file(&path, session_id)?;
            remove_file(&sidecar_path_for(&path), session_id)?;
        }
        session_index::remove_thread_name_entries(&self.dir, session_id)?;
        if found {
            Ok(())
        } else {
            Err(not_found(session_id))
        }
    }
}

/// Moves a rollout back to where it was, as Codex restores the moves its index could not follow.
fn restore(moved: &Path, original: &Path, session_id: &SessionId) {
    if let Err(error) = move_rollout(moved, original) {
        tracing::warn!(%session_id, %error, "failed to restore a moved rollout");
    }
}

/// Runs `work` to completion on a blocking task, whether or not the caller is still waiting.
pub(crate) async fn run_to_completion<T, F>(work: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(work).await.map_err(|error| {
        Error::session(
            SessionErrorKind::Io,
            format!("a thread file operation did not complete: {error}"),
        )
    })?
}

/// Moves a rollout and the sidecar beside it without replacing a rollout already at
/// `destination`, putting the rollout back if the sidecar cannot follow, as Codex restores the
/// moves it made when a later one fails.
fn move_rollout(source: &Path, destination: &Path) -> io::Result<()> {
    rename_no_replace(source, destination)?;
    match std::fs::rename(sidecar_path_for(source), sidecar_path_for(destination)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            if let Err(restore) = rename_no_replace(destination, source) {
                tracing::warn!(path = %source.display(), %restore, "failed to restore a moved rollout");
            }
            Err(error)
        }
    }
}

/// Renames `source` to `destination`, failing with [`io::ErrorKind::AlreadyExists`] rather than
/// replacing what is there. Where the file system cannot do that atomically, the destination is
/// checked first; the thread's writer lock keeps this directory's own writers out meanwhile.
fn rename_no_replace(source: &Path, destination: &Path) -> io::Result<()> {
    #[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android"))]
    {
        use rustix::fs::{CWD, RenameFlags, renameat_with};
        match renameat_with(CWD, source, CWD, destination, RenameFlags::NOREPLACE) {
            Ok(()) => return Ok(()),
            Err(error) if error == rustix::io::Errno::EXIST => {
                return Err(io::Error::from(io::ErrorKind::AlreadyExists));
            }
            // A file system or kernel without the flag falls back to the check below.
            Err(error)
                if error == rustix::io::Errno::INVAL || error == rustix::io::Errno::NOSYS => {}
            Err(error) => return Err(io::Error::from(error)),
        }
    }
    if std::fs::symlink_metadata(destination).is_ok() {
        return Err(io::Error::from(io::ErrorKind::AlreadyExists));
    }
    std::fs::rename(source, destination)
}

/// Whether `path` is a regular file; anything else may never end when read.
fn is_file(path: &Path, session_id: &SessionId) -> Result<bool> {
    match std::fs::metadata(path) {
        Ok(metadata) => Ok(metadata.is_file()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(lookup_error(session_id, &error)),
    }
}

/// Removes `path`, reporting whether it was there.
fn remove_file(path: &Path, session_id: &SessionId) -> Result<bool> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(Error::session(
            SessionErrorKind::Io,
            format!(
                "failed to delete `{}` of session `{session_id}`: {error}",
                path.display()
            ),
        )),
    }
}

fn is_not_found(error: &Error) -> bool {
    matches!(
        error,
        Error::Session {
            kind: SessionErrorKind::NotFound,
            ..
        }
    )
}

pub(crate) fn not_found(session_id: &SessionId) -> Error {
    Error::session(
        SessionErrorKind::NotFound,
        format!("no rollout of session `{session_id}` is kept here"),
    )
}

pub(crate) fn lookup_error(session_id: &SessionId, error: &io::Error) -> Error {
    Error::session(
        SessionErrorKind::Io,
        format!("failed to look up the rollout of session `{session_id}`: {error}"),
    )
}

fn move_error(action: &str, session_id: &SessionId, error: &io::Error) -> Error {
    Error::session(
        SessionErrorKind::Io,
        format!("failed to {action} the thread of session `{session_id}`: {error}"),
    )
}
