//! The rollouts of an agent tree's threads, side by side in one directory.
//!
//! Ported from Codex's local thread store as it serves spawned threads. Every agent a tree spawns
//! is a thread with a rollout of its own, named after its session as any rollout here is, whose
//! first record is session metadata saying where the thread was spawned from: Codex's
//! `SessionMeta` with its `ThreadSpawn` source. The threads spawned from one session are found by
//! reading that metadata, as Codex lists a thread's children by filtering its rollouts' metadata
//! on the parent thread.
//!
//! The directory is also where a root thread or a fork is created, where a thread is resumed from,
//! read back and listed, and where it is renamed, archived and deleted, as Codex's local store does
//! with its rollouts: it is a [`ThreadStore`]. Archived rollouts move, under the same name, into an
//! `archived_sessions` directory inside this one, as Codex keeps them flat in its own; names are
//! kept in a session index beside the rollouts. Given a state database
//! (`RolloutThreadDirectory::with_state_db`, with the `sqlite` feature), the directory also keeps
//! its threads' metadata there and lists threads from it, as Codex's local store does with its
//! own; see `ra_session::store::local`. Without one, it works as Codex's local store does without
//! its state database.
//!
//! # Differences from Codex
//!
//! Codex's store owns its live writers, so deleting a thread it is writing discards the writer
//! first. A writer here is the recorder its caller holds, which the store cannot stop, so a thread
//! a live writer holds is not archived, unarchived or deleted; the writer has to let go first. As
//! Codex's local store does, archiving or deleting several threads takes every writer lock before
//! touching any file, so one busy thread fails the batch before anything moves.
//!
//! Codex moves an archived rollout over whatever is at its destination. A move here never replaces
//! a rollout, so a thread archived again after its id was reused cannot overwrite the earlier
//! archived history; both ways of creating a thread refuse an id with an archived rollout, too.
//! Codex's reference checks before deleting a thread guard histories that its paginated forks share
//! with their source; forks here copy their history, so there is nothing to check. A resume
//! reopens active threads only. Without a state database, of a metadata patch only the name is
//! kept, as Codex keeps only the name without its state database; the rest is read from the rollout
//! when the thread is.

use std::{
    collections::HashSet,
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
    session_index,
    thread_files::{ThreadFiles, lookup_error, not_found, run_to_completion},
    writer::{RolloutSessionMeta, RolloutWriter, session_rollout_path},
    writer_lock::{WriterLockCoordinator, WriterLockGuard},
};
use crate::{
    lite::{self, modified_time, read_head_summary},
    store::{
        ArchiveThreadParams, ArchiveThreadsParams, CreateThreadParams, DeleteThreadParams,
        DeleteThreadsParams, ListThreadsParams, LoadThreadHistoryParams, ReadThreadParams,
        ResumeThreadParams, StoredThread, StoredThreadHistory, ThreadPage, ThreadStore,
        UpdateThreadMetadataParams, index::ThreadIndex, initial_payloads, is_not_found,
        live::LiveThreadRecorder,
    },
};

/// A directory holding the rollouts of the threads an agent tree spawns, and usually its root's.
///
/// As a [`RolloutThreadStore`], it gives each spawned agent's thread the rollout
/// `rollout-<session id>.jsonl` here, recorded through a [`RolloutFileRecorder`] that writes the
/// thread's session metadata first. The recorder handed back derives the thread's metadata from
/// what is recorded, as Codex's live thread does; the directory keeps it in its state database, if
/// it has one, and otherwise keeps none of it but names. The file is created when the agent's first
/// run records into it, as Codex defers creating a rollout until something is persisted.
#[derive(Debug, Clone)]
pub struct RolloutThreadDirectory {
    dir: PathBuf,
    writer_locks: Arc<WriterLockCoordinator>,
    index: Option<Arc<dyn ThreadIndex>>,
}

impl RolloutThreadDirectory {
    /// The rollouts in `dir`, which is created when the first of them is.
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        Self {
            writer_locks: Arc::new(WriterLockCoordinator::new(&dir)),
            dir,
            index: None,
        }
    }

    /// Keeps the directory's threads in `state_db` as well, as Codex's local store keeps them in
    /// its state database: see [`crate::store::local`]. `state_db` should be the database
    /// [`crate::store::local::init`] opened for this directory.
    #[cfg(feature = "sqlite")]
    #[must_use]
    pub fn with_state_db(mut self, state_db: Arc<crate::store::local::StateRuntime>) -> Self {
        self.index = Some(state_db);
        self
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

    /// Where archived rollouts are kept: Codex's `archived_sessions`.
    fn archived_dir(&self) -> PathBuf {
        self.files().archived_dir()
    }

    fn files(&self) -> ThreadFiles {
        ThreadFiles::new(&self.dir, self.index.clone())
    }

    /// The writer locks of `session_ids`, all or none, as Codex's local store takes every lock of
    /// a batch before it moves or deletes anything.
    fn acquire_all(&self, session_ids: &[SessionId]) -> Result<Vec<Arc<WriterLockGuard>>> {
        let mut ordered = session_ids.iter().collect::<Vec<_>>();
        ordered.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        ordered.dedup();
        ordered
            .into_iter()
            .map(|session_id| self.writer_locks.acquire(session_id))
            .collect()
    }

    /// The rollout of `session_id`, which must be a file here.
    async fn existing_rollout(&self, session_id: &SessionId) -> Result<PathBuf> {
        let path = self.rollout_path(session_id)?;
        if is_file(&path, session_id).await? {
            Ok(path)
        } else {
            Err(not_found(session_id))
        }
    }

    /// The archived rollout of `session_id`, if there is one.
    async fn archived_rollout(&self, session_id: &SessionId) -> Result<Option<PathBuf>> {
        let path = session_rollout_path(&self.archived_dir(), session_id)?;
        Ok(is_file(&path, session_id).await?.then_some(path))
    }

    /// The rollout of `session_id` and whether it is archived: the active one, or with
    /// `include_archived` the archived one when there is no active one.
    async fn locate(
        &self,
        session_id: &SessionId,
        include_archived: bool,
    ) -> Result<(PathBuf, bool)> {
        match self.existing_rollout(session_id).await {
            Ok(path) => Ok((path, false)),
            Err(error) if include_archived && is_not_found(&error) => self
                .archived_rollout(session_id)
                .await?
                .map(|path| (path, true))
                .ok_or(error),
            Err(error) => Err(error),
        }
    }

    /// Fills in what a listing shows of the thread whose rollout is `path`: the preview and first
    /// user message from its head, when it was last written to and archived, and its name.
    async fn describe(&self, thread: &mut StoredThread, path: &Path, archived: bool) -> Result<()> {
        let head = read_head_summary(path).await?;
        if let Some(described) = head.into_thread() {
            if thread.created_at().is_none()
                && let Some(created_at) = described.created_at()
            {
                thread.set_created_at(created_at);
            }
            thread.set_head(
                described.preview().to_owned(),
                described.first_user_message().map(str::to_owned),
            );
        }
        if let Some(modified) = modified_time(path).await {
            thread.set_updated_at(modified);
            if archived {
                thread.set_archived_at(modified);
            }
        }
        let ids = HashSet::from([thread.session_id().clone()]);
        if let Some(name) = session_index::find_thread_names(&self.dir, &ids)
            .await?
            .remove(thread.session_id())
        {
            thread.set_name(name);
        }
        if let Some(index) = &self.index {
            index.overlay(thread).await;
        }
        Ok(())
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
        self.files().ensure_new(session_id)?;
        let meta = RolloutSessionMeta::new(session_id.clone()).with_thread_spawn(spawn.clone());
        let recorder = Arc::new(RolloutFileRecorder::create_with_ownership(
            path,
            meta.clone(),
            ownership,
        ));
        Ok(LiveThreadRecorder::created(
            Arc::new(self.clone()),
            recorder,
            &meta,
            &[],
        ))
    }
}

#[async_trait]
impl ThreadStore for RolloutThreadDirectory {
    /// Takes the thread's writer lock at once, then refuses a session whose rollout is already
    /// here, active or archived, as the spawn form does, and queues the history behind the session
    /// metadata; the file is created when something is persisted. Holding the lock while checking
    /// keeps a second creator from passing the same check before the file exists.
    async fn create_thread_with(
        &self,
        params: &CreateThreadParams,
    ) -> Result<Arc<dyn RolloutRecorder>> {
        let history = initial_payloads(params.history())?;
        let path = self.rollout_path(params.session_id())?;
        let ownership = self.writer_locks.acquire(params.session_id())?;
        self.files().ensure_new(params.session_id())?;
        let recorder = Arc::new(RolloutFileRecorder::create_with_history(
            path,
            params.meta().clone(),
            history.clone(),
            ownership,
        ));
        Ok(LiveThreadRecorder::created(
            Arc::new(self.clone()),
            recorder,
            params.meta(),
            &history,
        ))
    }

    /// Reopens the thread's rollout file at once, as Codex's local store reopens its live writer
    /// on resume, so a live writer still holding it is reported here rather than at the first
    /// record. Reopening repairs a torn last line and continues the sequence where the file ends.
    ///
    /// Without supplied history the reopened file is read for what its metadata derives from, as
    /// Codex's live thread loads the history on resume; if it cannot be read, the writer is
    /// discarded and the error returned.
    async fn resume_thread(&self, params: &ResumeThreadParams) -> Result<Arc<dyn RolloutRecorder>> {
        let ownership = self.writer_locks.acquire(params.session_id())?;
        let path = self.existing_rollout(params.session_id()).await?;
        let writer = RolloutWriter::open_with_ownership(
            path.clone(),
            params.session_id().clone(),
            ownership,
        )
        .await?;
        let recorder: Arc<dyn RolloutRecorder> = Arc::new(RolloutFileRecorder::spawn(writer));
        let history = match params.history() {
            Some(history) => history.to_vec(),
            None => match RolloutReader::open(&path).read_all().await {
                Ok(history) => history,
                Err(error) => {
                    if let Err(discard) = recorder.discard().await {
                        tracing::warn!(
                            session_id = %params.session_id(),
                            error = %discard,
                            "failed to discard a resumed writer after its history could not be read"
                        );
                    }
                    return Err(error);
                }
            },
        };
        Ok(LiveThreadRecorder::resumed(
            Arc::new(self.clone()),
            recorder,
            params.session_id().clone(),
            &history,
        ))
    }

    async fn load_history(&self, params: &LoadThreadHistoryParams) -> Result<StoredThreadHistory> {
        let (path, _) = self
            .locate(params.session_id(), params.include_archived())
            .await?;
        let records = RolloutReader::open(path).read_all().await?;
        Ok(StoredThreadHistory::new(
            params.session_id().clone(),
            records,
        ))
    }

    /// Reads the thread's session metadata, and what a listing shows of it from the head of its
    /// rollout, its modification time and the session index.
    async fn read_thread(&self, params: &ReadThreadParams) -> Result<StoredThread> {
        let (path, archived) = self
            .locate(params.session_id(), params.include_archived())
            .await?;
        let reader = RolloutReader::open(&path);
        let mut thread = if params.include_history() {
            let history =
                StoredThreadHistory::new(params.session_id().clone(), reader.read_all().await?);
            StoredThread::from_history(history)?
        } else {
            let meta = reader.session_meta().await?;
            StoredThread::new(params.session_id().clone(), meta.as_ref())
        };
        self.describe(&mut thread, &path, archived).await?;
        Ok(thread.with_rollout_path(path))
    }

    /// Without a state database, reads the head of every rollout, as Codex's local store does
    /// without one; a listing of the state database alone then lists nothing, as Codex's does.
    /// With one, see `ra_session::store::local`.
    async fn list_threads(&self, params: &ListThreadsParams) -> Result<ThreadPage> {
        let dir = if params.is_archived() {
            self.archived_dir()
        } else {
            self.dir.clone()
        };
        match &self.index {
            Some(index) => index.list_threads(&dir, &self.dir, params).await,
            None if params.use_state_db_only() => Ok(ThreadPage::new(Vec::new(), None)),
            None => lite::list_threads(&dir, &self.dir, params.is_archived(), params).await,
        }
    }

    /// Keeps a name in the session index; an empty or cleared name clears it. The other fields
    /// are kept only in a state database, as Codex keeps them.
    async fn update_thread_metadata(
        &self,
        params: &UpdateThreadMetadataParams,
    ) -> Result<Option<StoredThread>> {
        let session_id = params.session_id();
        let (path, archived) = self.locate(session_id, params.include_archived()).await?;
        if let Some(index) = &self.index {
            index
                .update_thread_metadata(session_id, params.patch(), path, archived)
                .await;
        }
        if let Some(name) = params.patch().name() {
            session_index::append_thread_name(&self.dir, session_id, name.unwrap_or_default())?;
        }
        let mut read = ReadThreadParams::new(session_id.clone());
        if params.include_archived() {
            read = read.including_archived();
        }
        self.read_thread(&read).await.map(Some)
    }

    /// Keeps a name as [`Self::update_thread_metadata`] does. A patch without one is not looked up
    /// against the rollouts: a live thread's recorder writes such patches before its rollout is
    /// created. It is written to a state database, where the thread is given a row if it has none,
    /// and otherwise has nothing here to keep, as Codex's local store writes nothing of it without
    /// its state database.
    async fn record_thread_metadata(&self, params: &UpdateThreadMetadataParams) -> Result<()> {
        if params.patch().name().is_some() {
            return self.update_thread_metadata(params).await.map(|_| ());
        }
        if let Some(index) = &self.index {
            let active_path = self.rollout_path(params.session_id())?;
            index
                .record_thread_metadata(params.session_id(), params.patch(), active_path)
                .await;
        }
        Ok(())
    }

    /// Moves the rollout, and its sidecar, into `archived_sessions` under the thread's writer lock.
    async fn archive_thread(&self, params: &ArchiveThreadParams) -> Result<()> {
        self.archive_threads(&ArchiveThreadsParams::new(vec![
            params.session_id().clone(),
        ]))
        .await
        .map(|_| ())
    }

    /// Codex's local `archive_threads`: every writer lock first, then the moves in order.
    async fn archive_threads(&self, params: &ArchiveThreadsParams) -> Result<Vec<SessionId>> {
        if params.session_ids().is_empty() {
            return Ok(Vec::new());
        }
        let ownership = self.acquire_all(params.session_ids())?;
        let files = self.files();
        let session_ids = params.session_ids().to_vec();
        run_to_completion(move || files.archive(&session_ids, ownership)).await
    }

    /// Moves the rollout, and its sidecar, back under the thread's writer lock and marks it
    /// modified now, so it lists as just updated, as Codex's does.
    async fn unarchive_thread(&self, params: &ArchiveThreadParams) -> Result<StoredThread> {
        let session_id = params.session_id().clone();
        let ownership = self.writer_locks.acquire(&session_id)?;
        let files = self.files();
        let unarchived = session_id.clone();
        run_to_completion(move || files.unarchive(&unarchived, ownership)).await?;
        self.read_thread(&ReadThreadParams::new(session_id)).await
    }

    /// Deletes the active and archived rollouts and their sidecars under the thread's writer lock,
    /// then the thread's names, as Codex's does whether or not a rollout was found.
    async fn delete_thread(&self, params: &DeleteThreadParams) -> Result<()> {
        self.delete_many(vec![params.session_id().clone()], false)
            .await
    }

    /// Codex's local `delete_threads`: every writer lock first, then the deletions in order, a
    /// thread already gone counting as deleted.
    async fn delete_threads(&self, params: &DeleteThreadsParams) -> Result<()> {
        self.delete_many(params.session_ids().to_vec(), true).await
    }
}

impl RolloutThreadDirectory {
    async fn delete_many(
        &self,
        session_ids: Vec<SessionId>,
        missing_is_deleted: bool,
    ) -> Result<()> {
        if session_ids.is_empty() {
            return Ok(());
        }
        let ownership = self.acquire_all(&session_ids)?;
        let files = self.files();
        run_to_completion(move || files.delete(&session_ids, missing_is_deleted, ownership)).await
    }
}

/// Whether `path` is a regular file; anything else may never end when read.
async fn is_file(path: &Path, session_id: &SessionId) -> Result<bool> {
    match tokio::fs::metadata(path).await {
        Ok(metadata) => Ok(metadata.is_file()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(lookup_error(session_id, &error)),
    }
}

fn list_error(error: &std::io::Error) -> Error {
    Error::session(
        SessionErrorKind::Io,
        format!("failed to list the rollout directory: {error}"),
    )
}
