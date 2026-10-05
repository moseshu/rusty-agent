//! The storage boundary of a session's threads: Codex's `ThreadStore`
//! (`thread-store/src/store.rs`).
//!
//! A thread is a session's rollout, written by a live writer while the thread runs. Creating a
//! thread hands back that writer through [`RolloutThreadStore`], in `ra-core`, so the runtime can
//! create the threads it spawns without knowing how they are stored. [`ThreadStore`] extends it
//! with resuming, including caller-supplied storage records, and reading the stored records:
//! loading a thread's history for a resume, and reading a thread. Two stores implement it: the
//! rollout directory, [`RolloutThreadDirectory`](crate::rollout::RolloutThreadDirectory), as
//! Codex's local store keeps one rollout file per thread, and [`InMemoryThreadStore`], as Codex
//! keeps an in-memory one.
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
//! metadata, sections, attachments, projects, search and queued submissions. Listing threads,
//! their metadata, archiving and deletion are yet to come.
//!
//! The `local`, `mirror` and `summary` modules below are empty and kept only because they were
//! released.

#[cfg(feature = "sqlite")]
pub mod local;
pub mod mirror;
pub mod summary;

mod in_memory;

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
        rollout::{RolloutRecorder, RolloutThreadSpawn, RolloutThreadStore},
    },
};

pub use in_memory::InMemoryThreadStore;

use crate::rollout::{RolloutPayload, RolloutRecord, RolloutSessionMeta};

/// Resumes and reads the threads a store holds: the rest of Codex's `ThreadStore` that this
/// framework ports.
#[async_trait]
pub trait ThreadStore: RolloutThreadStore {
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
}

impl LoadThreadHistoryParams {
    /// Loads the history of the thread of `session_id`.
    #[must_use]
    pub const fn new(session_id: SessionId) -> Self {
        Self { session_id }
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
}

impl ReadThreadParams {
    /// Reads the thread of `session_id`, without its records.
    #[must_use]
    pub const fn new(session_id: SessionId) -> Self {
        Self {
            session_id,
            include_history: false,
        }
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

/// A thread as a store reads it: Codex's `StoredThread`, so far with what a thread's session
/// metadata records.
///
/// Codex's other fields — preview, name, update and archive times, the model and its settings,
/// the token usage and the first user message — come from metadata derived as records are appended,
/// which is not ported yet. A thread whose rollout records no session metadata, such as a root
/// thread whose recorder was created without it, has none of the metadata here.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct StoredThread {
    session_id: SessionId,
    rollout_path: Option<PathBuf>,
    thread_spawn: Option<RolloutThreadSpawn>,
    created_at: Option<EventTimestamp>,
    cwd: Option<String>,
    model_provider: Option<String>,
    originator: Option<String>,
    cli_version: Option<String>,
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
            created_at: meta.and_then(RolloutSessionMeta::created_at),
            cwd: meta.and_then(|meta| meta.cwd().map(str::to_owned)),
            model_provider: meta.and_then(|meta| meta.model_provider().map(str::to_owned)),
            originator: meta.and_then(|meta| meta.originator().map(str::to_owned)),
            cli_version: meta.and_then(|meta| meta.cli_version().map(str::to_owned)),
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

    /// The thread's records, when they were read.
    #[must_use]
    pub const fn history(&self) -> Option<&StoredThreadHistory> {
        self.history.as_ref()
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
