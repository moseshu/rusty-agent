//! The storage boundary of a session's threads: Codex's `ThreadStore`
//! (`thread-store/src/store.rs`).
//!
//! A thread is a session's rollout, written by a live writer while the thread runs. Creating a
//! thread hands back that writer through [`RolloutThreadStore`], in `ra-core`, so the runtime can
//! create the threads it spawns without knowing how they are stored. [`ThreadStore`] extends it
//! with creating a thread from its whole session metadata and an initial history, as a fork is
//! created; resuming, including caller-supplied storage records; reading the stored records:
//! loading a thread's history for a resume or a fork, reading a thread and listing threads; and
//! managing them: changing their metadata, archiving, unarchiving and deleting them. Two stores
//! implement it: the rollout directory,
//! [`RolloutThreadDirectory`](crate::rollout::RolloutThreadDirectory), as Codex's local store
//! keeps one rollout file per thread, and [`InMemoryThreadStore`], as Codex keeps an in-memory
//! one.
//!
//! # What is not ported
//!
//! Codex's store is addressed by thread id and owns each live writer; here the writer is the
//! recorder the store hands back, so persisting, flushing, shutting down and discarding a live
//! thread go through it rather than through the store.
//!
//! Codex has two history modes. Only its legacy mode is ported, where the rollout is the history;
//! its paginated mode — history projected into `SQLite` turn and item tables, with reads of the
//! latest model context, reference-backed forks, reverts and turn, item and timeline listings —
//! is not, and neither are the methods Codex's store gives an `Unsupported` default: staged
//! metadata, sections, attachments, projects, search and queued submissions.
//!
//! Codex's local store keeps an `SQLite` state database beside its rollouts. Here the directory
//! keeps one only when it is given one (the `local` module, with the `sqlite` feature); without
//! it, listing reads the rollouts themselves, as Codex's local store does without one, and of the
//! metadata a patch carries only the name is kept by the directory.
//!
//! # Live threads
//!
//! Codex's `LiveThread` derives a thread's metadata from what is appended to it — the first user
//! message, preview and title, the latest model and effort, the working directory, the update time
//! — and writes it through the store's `record_thread_metadata`. Here the recorder both stores hand
//! back for a created, spawned or resumed thread does the same; a caller using a
//! [`RolloutFileRecorder`](crate::rollout::RolloutFileRecorder) directly gets none of it. The
//! directory keeps these fields only in its state database, as Codex's local store does; the
//! in-memory store keeps them all.
//!
//! The `mirror` and `summary` modules below are empty and kept only because they were released.

#[cfg(feature = "sqlite")]
pub mod local;
pub mod mirror;
pub mod summary;

mod in_memory;
pub(crate) mod index;
mod list;
pub(crate) mod live;
mod metadata;
mod metadata_sync;

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use ra_core::{
    error::Result,
    event::EventTimestamp,
    session::{
        SessionId,
        rollout::{RolloutItem, RolloutRecorder, RolloutThreadSpawn, RolloutThreadStore},
    },
};

pub use in_memory::InMemoryThreadStore;
pub use list::{ListThreadsParams, SortDirection, ThreadPage, ThreadSortKey};
pub use metadata::{
    ArchiveThreadParams, ArchiveThreadsParams, DeleteThreadParams, DeleteThreadsParams,
    ThreadMetadataPatch, UpdateThreadMetadataParams,
};

use crate::rollout::{
    RolloutPayload, RolloutRecord, RolloutSessionMeta, recorder::is_persisted_rollout_item,
};

/// Creates, resumes, reads, lists and manages the threads a store holds: the rest of Codex's
/// `ThreadStore` that this framework ports.
#[async_trait]
pub trait ThreadStore: RolloutThreadStore {
    /// Creates the thread `params` describes and returns the recorder its runs record through:
    /// Codex's `create_thread`, followed by the `append_items` its session makes with a forked
    /// history before anything else.
    ///
    /// The supertrait's [`RolloutThreadStore::create_thread`] is the form the runtime uses for the
    /// agents it spawns. This one takes the thread's whole session metadata, so it can create a
    /// root thread or a fork, and the records the thread starts with. They are written right after
    /// the session metadata, renumbered, through the shared persistence policy; a store's
    /// checkpoints summarize the file they were written in and are left out. As with the spawn
    /// form, the directory defers creating the file until something is persisted.
    ///
    /// The thread must be new: a session id the store already holds is refused before anything is
    /// written, so the history it holds is never appended to. Codex never meets such an id, since
    /// it generates the id of every thread it creates; here a caller can choose one.
    ///
    /// # Errors
    ///
    /// Returns an error if the store already holds a thread of the session id or a live writer
    /// holds it, if a history record cannot be read, or if the session id cannot name a rollout.
    async fn create_thread_with(
        &self,
        params: &CreateThreadParams,
    ) -> Result<Arc<dyn RolloutRecorder>>;

    /// Reopens a thread's writer: Codex's `resume_thread`. Local stores acquire exclusive thread
    /// ownership before opening the file and retain it throughout I/O recovery. In-memory stores
    /// install supplied history or preserve existing history, creating an empty history if absent.
    ///
    /// # Errors
    ///
    /// Returns an error of kind [`SessionErrorKind::NotFound`](ra_core::error::SessionErrorKind)
    /// when a local rollout is missing, or an error if its writer cannot be opened.
    async fn resume_thread(&self, params: &ResumeThreadParams) -> Result<Arc<dyn RolloutRecorder>>;

    /// The records of a thread, in the order they were written, for a resume or a fork: Codex's
    /// `load_history`. They are the input of
    /// [`reconstruct_history`](crate::rollout::reconstruct_history).
    ///
    /// # Errors
    ///
    /// Returns an error of kind [`SessionErrorKind::NotFound`](ra_core::error::SessionErrorKind) if
    /// the store holds no such thread, or an error if its records cannot be read.
    async fn load_history(&self, params: &LoadThreadHistoryParams) -> Result<StoredThreadHistory>;

    /// A thread, and with [`ReadThreadParams::with_history`] its records: Codex's `read_thread`.
    ///
    /// # Errors
    ///
    /// Returns an error of kind [`SessionErrorKind::NotFound`](ra_core::error::SessionErrorKind) if
    /// the store holds no such thread, or an error if it cannot be read.
    async fn read_thread(&self, params: &ReadThreadParams) -> Result<StoredThread>;

    /// A page of the threads `params` selects: Codex's `list_threads`.
    ///
    /// # Errors
    ///
    /// Returns an error if the cursor is not one a listing returned, or if the threads cannot be
    /// listed.
    async fn list_threads(&self, params: &ListThreadsParams) -> Result<ThreadPage>;

    /// Changes a thread's metadata and returns the thread as it now reads: Codex's
    /// `update_thread_metadata`. `None` means the change succeeded without the store reading the
    /// thread back.
    ///
    /// The store applies what the patch sets as it is given; deciding what to derive from a
    /// thread's records belongs above it.
    ///
    /// # Errors
    ///
    /// Returns an error of kind [`SessionErrorKind::NotFound`](ra_core::error::SessionErrorKind) if
    /// the store holds no such thread, or an error if the change cannot be kept.
    async fn update_thread_metadata(
        &self,
        params: &UpdateThreadMetadataParams,
    ) -> Result<Option<StoredThread>>;

    /// Records metadata derived from a thread's records: Codex's `record_thread_metadata`, which
    /// the recorders of live threads write through. Unlike [`Self::update_thread_metadata`] it
    /// does not read the thread back.
    ///
    /// The default applies the patch through [`Self::update_thread_metadata`]. A store may defer
    /// the write only if the recorders it hands back wait for it when flushed and shut down, as
    /// Codex's deferred writes respect its flush and shutdown guarantees.
    ///
    /// # Errors
    ///
    /// Returns an error if the change cannot be kept.
    async fn record_thread_metadata(&self, params: &UpdateThreadMetadataParams) -> Result<()> {
        self.update_thread_metadata(params).await.map(|_| ())
    }

    /// Archives a thread: Codex's `archive_thread`.
    ///
    /// # Errors
    ///
    /// Returns an error if the store holds no active thread of the session, if a live writer
    /// holds it, or if it cannot be moved.
    async fn archive_thread(&self, params: &ArchiveThreadParams) -> Result<()>;

    /// Archives threads in order and returns the sessions of those archived: Codex's
    /// `archive_threads`. The first must be archived; a later failure is logged and skipped.
    ///
    /// # Errors
    ///
    /// Returns the error of the first thread if it cannot be archived.
    async fn archive_threads(&self, params: &ArchiveThreadsParams) -> Result<Vec<SessionId>> {
        let mut archived = Vec::new();
        for session_id in params.session_ids() {
            match self
                .archive_thread(&ArchiveThreadParams::new(session_id.clone()))
                .await
            {
                Ok(()) => archived.push(session_id.clone()),
                Err(error) if archived.is_empty() => return Err(error),
                Err(error) => {
                    tracing::warn!(%session_id, %error, "failed to archive a thread");
                }
            }
        }
        Ok(archived)
    }

    /// Unarchives a thread and returns it as it now reads: Codex's `unarchive_thread`.
    ///
    /// # Errors
    ///
    /// Returns an error if the store holds no archived thread of the session, if a live writer
    /// holds it, or if it cannot be moved back.
    async fn unarchive_thread(&self, params: &ArchiveThreadParams) -> Result<StoredThread>;

    /// Deletes a thread's records and what the store keeps about it: Codex's `delete_thread`.
    ///
    /// # Errors
    ///
    /// Returns an error of kind [`SessionErrorKind::NotFound`](ra_core::error::SessionErrorKind) if
    /// the store holds no such thread, or an error if a live writer holds it or it cannot be
    /// deleted.
    async fn delete_thread(&self, params: &DeleteThreadParams) -> Result<()>;

    /// Deletes threads in order, a thread already gone counting as deleted: Codex's
    /// `delete_threads`.
    ///
    /// # Errors
    ///
    /// Returns the first error other than a thread being missing.
    async fn delete_threads(&self, params: &DeleteThreadsParams) -> Result<()> {
        for session_id in params.session_ids() {
            match self
                .delete_thread(&DeleteThreadParams::new(session_id.clone()))
                .await
            {
                Ok(()) => {}
                Err(error) if is_not_found(&error) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
}

/// Whether `error` says the store holds no such thread.
pub(crate) fn is_not_found(error: &ra_core::error::Error) -> bool {
    matches!(
        error,
        ra_core::error::Error::Session {
            kind: ra_core::error::SessionErrorKind::NotFound,
            ..
        }
    )
}

/// The thread [`ThreadStore::create_thread_with`] creates: Codex's `CreateThreadParams`, with the
/// history its session appends first.
///
/// Codex's parameters carry the pieces of the thread's `SessionMeta`; here they are that metadata,
/// whose session id is the thread's. Codex appends a forked history through `append_items` once
/// the thread exists; here a thread's records are written through its recorder, which takes only
/// what a run records, so the history is handed over at creation.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct CreateThreadParams {
    meta: RolloutSessionMeta,
    history: Vec<RolloutRecord>,
}

impl CreateThreadParams {
    /// Creates the thread `meta` describes, with no history.
    #[must_use]
    pub const fn new(meta: RolloutSessionMeta) -> Self {
        Self {
            meta,
            history: Vec::new(),
        }
    }

    /// Sets the records the thread starts with, written after its session metadata.
    #[must_use]
    pub fn with_history(mut self, history: Vec<RolloutRecord>) -> Self {
        self.history = history;
        self
    }

    /// The thread's session.
    #[must_use]
    pub const fn session_id(&self) -> &SessionId {
        self.meta.session_id()
    }

    /// The thread's session metadata, its first record.
    #[must_use]
    pub const fn meta(&self) -> &RolloutSessionMeta {
        &self.meta
    }

    /// The records the thread starts with.
    #[must_use]
    pub fn history(&self) -> &[RolloutRecord] {
        &self.history
    }
}

/// Which thread to reopen, with known replay history: Codex's `ResumeThreadParams`.
///
/// This lives beside `ThreadStore` because replay records, including session metadata and unknown
/// payloads, belong to storage rather than the runtime's provider-neutral recording port. The
/// local directory resolves paths by session id and only supports legacy history; its writer
/// resumes the persisted file. The in-memory store replaces its records with supplied history,
/// preserving the distinction between absent history and an explicitly empty history.
/// Archived selection and metadata for derived write projections are not implemented yet.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct ResumeThreadParams {
    session_id: SessionId,
    history: Option<Arc<Vec<RolloutRecord>>>,
}

impl ResumeThreadParams {
    /// Reopens the thread of `session_id` without supplying replay history.
    #[must_use]
    pub const fn new(session_id: SessionId) -> Self {
        Self {
            session_id,
            history: None,
        }
    }

    /// Supplies already loaded replay history, replacing the in-memory store's current records.
    #[must_use]
    pub fn with_history(mut self, history: impl Into<Arc<Vec<RolloutRecord>>>) -> Self {
        self.history = Some(history.into());
        self
    }

    /// The thread's session.
    #[must_use]
    pub const fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// Known replay history, if supplied.
    #[must_use]
    pub fn history(&self) -> Option<&[RolloutRecord]> {
        self.history.as_deref().map(Vec::as_slice)
    }
}

/// Which thread [`ThreadStore::load_history`] loads: Codex's `LoadThreadHistoryParams`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadThreadHistoryParams {
    session_id: SessionId,
    include_archived: bool,
}

impl LoadThreadHistoryParams {
    /// Loads the history of the active thread of `session_id`.
    #[must_use]
    pub const fn new(session_id: SessionId) -> Self {
        Self {
            session_id,
            include_archived: false,
        }
    }

    /// Loads the history of an archived thread as well: Codex's `include_archived`.
    #[must_use]
    pub const fn including_archived(mut self) -> Self {
        self.include_archived = true;
        self
    }

    /// Whether an archived thread's history may be loaded.
    #[must_use]
    pub const fn include_archived(&self) -> bool {
        self.include_archived
    }

    /// The thread's session.
    #[must_use]
    pub const fn session_id(&self) -> &SessionId {
        &self.session_id
    }
}

/// Which thread [`ThreadStore::read_thread`] reads, and whether with its records: Codex's
/// `ReadThreadParams`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadThreadParams {
    session_id: SessionId,
    include_history: bool,
    include_archived: bool,
}

impl ReadThreadParams {
    /// Reads the thread of `session_id`, without its records.
    #[must_use]
    pub const fn new(session_id: SessionId) -> Self {
        Self {
            session_id,
            include_history: false,
            include_archived: false,
        }
    }

    /// Reads an archived thread as well: Codex's `include_archived`.
    #[must_use]
    pub const fn including_archived(mut self) -> Self {
        self.include_archived = true;
        self
    }

    /// Whether an archived thread may be read.
    #[must_use]
    pub const fn include_archived(&self) -> bool {
        self.include_archived
    }

    /// Reads the thread's records as well: Codex's `include_history`.
    #[must_use]
    pub const fn with_history(mut self) -> Self {
        self.include_history = true;
        self
    }

    /// The thread's session.
    #[must_use]
    pub const fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// Whether the thread's records are read as well.
    #[must_use]
    pub const fn include_history(&self) -> bool {
        self.include_history
    }
}

/// A thread's records, in the order they were written: Codex's `StoredThreadHistory`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct StoredThreadHistory {
    session_id: SessionId,
    records: Vec<RolloutRecord>,
}

impl StoredThreadHistory {
    /// The records `records` of the thread of `session_id`.
    #[must_use]
    pub const fn new(session_id: SessionId, records: Vec<RolloutRecord>) -> Self {
        Self {
            session_id,
            records,
        }
    }

    /// The thread's session.
    #[must_use]
    pub const fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// The records.
    #[must_use]
    pub fn records(&self) -> &[RolloutRecord] {
        &self.records
    }

    /// The records, taken.
    #[must_use]
    pub fn into_records(self) -> Vec<RolloutRecord> {
        self.records
    }
}

/// A thread as a store reads it: Codex's `StoredThread`, with the fields this framework's records
/// have.
///
/// The session metadata describes the thread; a thread whose rollout records none, such as a root
/// thread whose recorder was created without it, has none of that here. The rest is what the
/// store knows besides: the name a caller gave it, the preview and first user message read from
/// the head of its records, when it was last written to and archived, and the model and effort a
/// store that keeps metadata patches was told of.
///
/// Codex's source, history mode, agent nickname and role, section, project, Git facts, approval
/// mode, permission profile, token usage and recency are its product's or its state database's,
/// and are not ported.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct StoredThread {
    session_id: SessionId,
    rollout_path: Option<PathBuf>,
    thread_spawn: Option<RolloutThreadSpawn>,
    forked_from_id: Option<SessionId>,
    created_at: Option<EventTimestamp>,
    cwd: Option<String>,
    model_provider: Option<String>,
    originator: Option<String>,
    cli_version: Option<String>,
    name: Option<String>,
    preview: String,
    first_user_message: Option<String>,
    updated_at: Option<EventTimestamp>,
    archived_at: Option<EventTimestamp>,
    model: Option<String>,
    effort: Option<String>,
    history: Option<StoredThreadHistory>,
}

impl StoredThread {
    /// The thread of `session_id`, described by `meta` if its rollout records session metadata.
    #[must_use]
    pub fn new(session_id: SessionId, meta: Option<&RolloutSessionMeta>) -> Self {
        Self {
            session_id,
            rollout_path: None,
            thread_spawn: meta.and_then(|meta| meta.thread_spawn().cloned()),
            forked_from_id: meta.and_then(|meta| meta.forked_from_id().cloned()),
            created_at: meta.and_then(RolloutSessionMeta::created_at),
            cwd: meta.and_then(|meta| meta.cwd().map(str::to_owned)),
            model_provider: meta.and_then(|meta| meta.model_provider().map(str::to_owned)),
            originator: meta.and_then(|meta| meta.originator().map(str::to_owned)),
            cli_version: meta.and_then(|meta| meta.cli_version().map(str::to_owned)),
            name: None,
            preview: String::new(),
            first_user_message: None,
            updated_at: None,
            archived_at: None,
            model: None,
            effort: None,
            history: None,
        }
    }

    /// The thread of `history`, described by the session metadata its first record holds, if
    /// any, and carrying `history`.
    ///
    /// # Errors
    ///
    /// Returns an error if the first record is session metadata this build cannot read.
    pub fn from_history(history: StoredThreadHistory) -> Result<Self> {
        let meta = first_session_meta(history.records())?;
        let mut thread = Self::new(history.session_id().clone(), meta.as_ref());
        thread.history = Some(history);
        Ok(thread)
    }

    /// Sets the file the thread's rollout is kept in.
    #[must_use]
    pub fn with_rollout_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.rollout_path = Some(path.into());
        self
    }

    /// Sets the thread's records.
    #[must_use]
    pub fn with_history(mut self, history: StoredThreadHistory) -> Self {
        self.history = Some(history);
        self
    }

    /// The thread's session: Codex's `thread_id`.
    #[must_use]
    pub const fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// The file the thread's rollout is kept in, when the store keeps it in one.
    #[must_use]
    pub fn rollout_path(&self) -> Option<&Path> {
        self.rollout_path.as_deref()
    }

    /// Where the thread was spawned from, if an agent tree spawned it: Codex's `source`, with its
    /// `agent_path` and `agent_role`.
    #[must_use]
    pub const fn thread_spawn(&self) -> Option<&RolloutThreadSpawn> {
        self.thread_spawn.as_ref()
    }

    /// The session of the thread this one was spawned from: Codex's `parent_thread_id`.
    #[must_use]
    pub fn parent_session_id(&self) -> Option<&SessionId> {
        self.thread_spawn
            .as_ref()
            .map(RolloutThreadSpawn::parent_session_id)
    }

    /// The session whose thread this one was forked from, for a forked thread: Codex's
    /// `forked_from_id`.
    #[must_use]
    pub const fn forked_from_id(&self) -> Option<&SessionId> {
        self.forked_from_id.as_ref()
    }

    /// When the thread was created.
    #[must_use]
    pub const fn created_at(&self) -> Option<EventTimestamp> {
        self.created_at
    }

    /// The working directory recorded for the thread.
    #[must_use]
    pub fn cwd(&self) -> Option<&str> {
        self.cwd.as_deref()
    }

    /// The model provider recorded for the thread.
    #[must_use]
    pub fn model_provider(&self) -> Option<&str> {
        self.model_provider.as_deref()
    }

    /// The originator recorded for the thread.
    #[must_use]
    pub fn originator(&self) -> Option<&str> {
        self.originator.as_deref()
    }

    /// The version of the program that created the thread.
    #[must_use]
    pub fn cli_version(&self) -> Option<&str> {
        self.cli_version.as_deref()
    }

    /// The user-facing name the thread was given, if any.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// The best available preview, usually the first user message; empty when there is none.
    #[must_use]
    pub fn preview(&self) -> &str {
        &self.preview
    }

    /// The first message from the user the thread holds, if it holds one.
    #[must_use]
    pub fn first_user_message(&self) -> Option<&str> {
        self.first_user_message.as_deref()
    }

    /// When the thread was last written to, if known.
    #[must_use]
    pub const fn updated_at(&self) -> Option<EventTimestamp> {
        self.updated_at
    }

    /// When the thread was archived, for an archived thread.
    #[must_use]
    pub const fn archived_at(&self) -> Option<EventTimestamp> {
        self.archived_at
    }

    /// The latest model, if the store was told of it.
    #[must_use]
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// The latest effort, if the store was told of it: Codex's `reasoning_effort`.
    #[must_use]
    pub fn effort(&self) -> Option<&str> {
        self.effort.as_deref()
    }

    /// The thread's records, when they were read.
    #[must_use]
    pub const fn history(&self) -> Option<&StoredThreadHistory> {
        self.history.as_ref()
    }
}

impl StoredThread {
    /// Sets what the head of the thread's records shows: the preview and the first user message.
    pub(crate) fn set_head(&mut self, preview: String, first_user_message: Option<String>) {
        self.preview = preview;
        self.first_user_message = first_user_message;
    }

    /// Sets when the thread was created.
    pub(crate) const fn set_created_at(&mut self, created_at: EventTimestamp) {
        self.created_at = Some(created_at);
    }

    /// Sets when the thread was last written to.
    pub(crate) const fn set_updated_at(&mut self, updated_at: EventTimestamp) {
        self.updated_at = Some(updated_at);
    }

    /// Marks the thread archived at `archived_at`.
    pub(crate) const fn set_archived_at(&mut self, archived_at: EventTimestamp) {
        self.archived_at = Some(archived_at);
    }

    /// Sets the name a caller gave the thread, unless it only repeats the preview: Codex's
    /// `set_thread_name` for a thread of its legacy history mode.
    pub(crate) fn set_name(&mut self, name: String) {
        if self.preview.trim() != name.trim() {
            self.name = Some(name);
        }
    }

    /// Applies what `patch` sets, as Codex's in-memory store reads a thread through the patches it
    /// was given.
    pub(crate) fn apply_patch(&mut self, patch: &ThreadMetadataPatch) {
        if let Some(name) = patch.name() {
            self.name = name.map(str::to_owned);
        }
        if let Some(preview) = patch.preview() {
            preview.clone_into(&mut self.preview);
        }
        if let Some(message) = patch.first_user_message() {
            self.first_user_message = Some(message.to_owned());
        }
        if let Some(provider) = patch.model_provider() {
            self.model_provider = Some(provider.to_owned());
        }
        if let Some(model) = patch.model() {
            self.model = Some(model.to_owned());
        }
        if let Some(effort) = patch.effort() {
            self.effort = effort.map(str::to_owned);
        }
        if let Some(created_at) = patch.created_at() {
            self.created_at = Some(created_at);
        }
        if let Some(updated_at) = patch.updated_at() {
            self.updated_at = Some(updated_at);
        }
        if let Some(cwd) = patch.cwd() {
            self.cwd = Some(cwd.to_owned());
        }
        if let Some(version) = patch.cli_version() {
            self.cli_version = Some(version.to_owned());
        }
        if let Some(originator) = patch.originator() {
            self.originator = Some(originator.to_owned());
        }
        if let Some(spawn) = patch.thread_spawn() {
            self.thread_spawn = Some(spawn.clone());
        }
        if let Some(source) = patch.forked_from_id() {
            self.forked_from_id = Some(source.clone());
        }
    }
}

/// The session metadata `records` open with, if they open with some. Only the first record is
/// read, as [`RolloutReader::session_meta`](crate::rollout::RolloutReader::session_meta) reads it.
pub(crate) fn first_session_meta(records: &[RolloutRecord]) -> Result<Option<RolloutSessionMeta>> {
    match records.first() {
        Some(record) if record.type_name() == "session_meta" => match record.payload()? {
            RolloutPayload::SessionMeta(meta) => Ok(Some(meta)),
            _ => Ok(None),
        },
        _ => Ok(None),
    }
}

/// What a thread created with `history` writes after its session metadata: each record's payload,
/// in order, without the checkpoints another file's writer left and without host events the
/// persistence policy drops, as Codex's `append_items` applies its policy.
pub(crate) fn initial_payloads(history: &[RolloutRecord]) -> Result<Vec<RolloutPayload>> {
    let mut payloads = Vec::with_capacity(history.len());
    for record in history {
        match record.payload()? {
            RolloutPayload::Checkpoint(_) => {}
            RolloutPayload::Event(event) => {
                let item = RolloutItem::Event(event);
                if is_persisted_rollout_item(&item) {
                    payloads.push(item.into());
                }
            }
            payload => payloads.push(payload),
        }
    }
    Ok(payloads)
}

/// The refusal of a thread [`ThreadStore::create_thread_with`] would create over one the store
/// already holds.
pub(crate) fn thread_exists(session_id: &SessionId) -> ra_core::error::Error {
    ra_core::error::Error::caller(format!(
        "the store already holds a thread of session `{session_id}`; a created thread needs a \
         session of its own"
    ))
}
