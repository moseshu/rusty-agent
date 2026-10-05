//! The rollouts of an agent tree's threads, side by side in one directory.
//!
//! Ported from Codex's local thread store as it serves spawned threads. Every agent a tree spawns
//! is a thread with a rollout of its own, named after its session as any rollout here is, whose
//! first record is session metadata saying where the thread was spawned from: Codex's
//! `SessionMeta` with its `ThreadSpawn` source. The threads spawned from one session are found by
//! reading that metadata, as Codex lists a thread's children by filtering its rollouts' metadata
//! on the parent thread.
//!
//! The directory is also where a thread is resumed from and read back, as Codex's local store
//! reopens and reads a thread's rollout: it is a [`ThreadStore`].

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use ra_core::{
    error::{Error, Result, SessionErrorKind},
    session::{
        SessionId,
        rollout::{RolloutRecorder, RolloutThreadSpawn, RolloutThreadStore},
    },
};

use super::{
    reader::RolloutReader,
    recorder::RolloutFileRecorder,
    writer::{RolloutSessionMeta, RolloutWriter, session_rollout_path},
    writer_lock::WriterLockCoordinator,
};
use crate::store::{
    LoadThreadHistoryParams, ReadThreadParams, ResumeThreadParams, StoredThread,
    StoredThreadHistory, ThreadStore,
};

/// A directory holding the rollouts of the threads an agent tree spawns, and usually its root's.
///
/// As a [`RolloutThreadStore`], it gives each spawned agent's thread the rollout
/// `rollout-<session id>.jsonl` here, recorded through a [`RolloutFileRecorder`] that writes the
/// thread's session metadata first. The file is created when the agent's first run records into
/// it, as Codex defers creating a rollout until something is persisted.
#[derive(Debug, Clone)]
pub struct RolloutThreadDirectory {
    dir: PathBuf,
    writer_locks: Arc<WriterLockCoordinator>,
}

impl RolloutThreadDirectory {
    /// The rollouts in `dir`, which is created when the first of them is.
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        Self {
            writer_locks: Arc::new(WriterLockCoordinator::new(&dir)),
            dir,
        }
    }

    /// The directory.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.dir
    }

    /// Where the rollout of `session_id` is kept here.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] if `session_id` holds characters that would leave the directory.
    pub fn rollout_path(&self, session_id: &SessionId) -> Result<PathBuf> {
        session_rollout_path(&self.dir, session_id)
    }

    /// The threads spawned from `parent`, oldest first, each with its session metadata: Codex's
    /// direct children of a thread.
    ///
    /// A rollout whose first record cannot be read is skipped, as Codex's listing skips one it
    /// cannot summarize, and so is one whose first record has not been written yet. Generated
    /// session ids sort by creation time, and so do the rollouts named after them.
    ///
    /// # Errors
    ///
    /// Returns [`Error`] if the directory exists but cannot be listed.
    pub async fn children(
        &self,
        parent: &SessionId,
    ) -> Result<Vec<(RolloutReader, RolloutSessionMeta)>> {
        let mut entries = match tokio::fs::read_dir(&self.dir).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(list_error(&error)),
        };
        let mut paths = Vec::new();
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|error| list_error(&error))?
        {
            // A rollout's sidecar ends in `.json` and is left out.
            let is_rollout = entry.file_name().to_str().is_some_and(|name| {
                name.starts_with("rollout-")
                    && Path::new(name)
                        .extension()
                        .is_some_and(|ext| ext == "jsonl")
            });
            // Only regular files: anything else may never end when read.
            if is_rollout
                && entry
                    .file_type()
                    .await
                    .is_ok_and(|file_type| file_type.is_file())
            {
                paths.push(entry.path());
            }
        }
        paths.sort();

        let mut children = Vec::new();
        for path in paths {
            let reader = RolloutReader::open(path);
            match reader.session_meta().await {
                Ok(Some(meta)) if meta.parent_session_id() == Some(parent) => {
                    children.push((reader, meta));
                }
                Ok(_) => {}
                Err(error) => tracing::warn!(
                    path = %reader.path().display(),
                    %error,
                    "a rollout's session metadata could not be read; it is not listed"
                ),
            }
        }
        Ok(children)
    }

    /// The rollout of `session_id`, which must be a file here.
    async fn existing_rollout(&self, session_id: &SessionId) -> Result<PathBuf> {
        let path = self.rollout_path(session_id)?;
        match tokio::fs::metadata(&path).await {
            // Only a regular file: anything else may never end when read.
            Ok(metadata) if metadata.is_file() => Ok(path),
            Ok(_) => Err(not_found(session_id)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Err(not_found(session_id))
            }
            Err(error) => Err(Error::session(
                SessionErrorKind::Io,
                format!("failed to look up the rollout of session `{session_id}`: {error}"),
            )
            .with_source(error)),
        }
    }
}

#[async_trait]
impl RolloutThreadStore for RolloutThreadDirectory {
    async fn create_thread(
        &self,
        session_id: &SessionId,
        spawn: &RolloutThreadSpawn,
    ) -> Result<Arc<dyn RolloutRecorder>> {
        let path = self.rollout_path(session_id)?;
        let ownership = self.writer_locks.acquire(session_id)?;
        let meta = RolloutSessionMeta::new(session_id.clone()).with_thread_spawn(spawn.clone());
        Ok(Arc::new(RolloutFileRecorder::create_with_ownership(
            path, meta, ownership,
        )))
    }
}

#[async_trait]
impl ThreadStore for RolloutThreadDirectory {
    /// Reopens the thread's rollout file at once, as Codex's local store reopens its live writer
    /// on resume, so a live writer still holding it is reported here rather than at the first
    /// record. Reopening repairs a torn last line and continues the sequence where the file ends.
    async fn resume_thread(&self, params: &ResumeThreadParams) -> Result<Arc<dyn RolloutRecorder>> {
        let ownership = self.writer_locks.acquire(params.session_id())?;
        let path = self.existing_rollout(params.session_id()).await?;
        let writer =
            RolloutWriter::open_with_ownership(path, params.session_id().clone(), ownership)
                .await?;
        Ok(Arc::new(RolloutFileRecorder::spawn(writer)))
    }

    async fn load_history(&self, params: &LoadThreadHistoryParams) -> Result<StoredThreadHistory> {
        let path = self.existing_rollout(params.session_id()).await?;
        let records = RolloutReader::open(path).read_all().await?;
        Ok(StoredThreadHistory::new(
            params.session_id().clone(),
            records,
        ))
    }

    async fn read_thread(&self, params: &ReadThreadParams) -> Result<StoredThread> {
        let path = self.existing_rollout(params.session_id()).await?;
        let reader = RolloutReader::open(&path);
        let thread = if params.include_history() {
            let history =
                StoredThreadHistory::new(params.session_id().clone(), reader.read_all().await?);
            StoredThread::from_history(history)?
        } else {
            let meta = reader.session_meta().await?;
            StoredThread::new(params.session_id().clone(), meta.as_ref())
        };
        Ok(thread.with_rollout_path(path))
    }
}

fn not_found(session_id: &SessionId) -> Error {
    Error::session(
        SessionErrorKind::NotFound,
        format!("no rollout of session `{session_id}` is kept here"),
    )
}

fn list_error(error: &std::io::Error) -> Error {
    Error::session(
        SessionErrorKind::Io,
        format!("failed to list the rollout directory: {error}"),
    )
}
