//! Threads kept in memory: Codex's `InMemoryThreadStore` (`thread-store/src/in_memory.rs`).

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard},
};

use async_trait::async_trait;
use ra_core::{
    error::{Error, Result, SessionErrorKind},
    event::EventTimestamp,
    session::{
        SessionId,
        rollout::{RolloutItem, RolloutRecorder, RolloutThreadSpawn, RolloutThreadStore},
    },
};

use super::{
    ArchiveThreadParams, CreateThreadParams, DeleteThreadParams, ListThreadsParams,
    LoadThreadHistoryParams, ReadThreadParams, ResumeThreadParams, StoredThread,
    StoredThreadHistory, ThreadMetadataPatch, ThreadPage, ThreadStore, UpdateThreadMetadataParams,
    first_session_meta, initial_payloads, live::LiveThreadRecorder, thread_exists,
};
use crate::rollout::{
    RolloutPayload, RolloutRecord, RolloutSessionMeta, recorder::is_persisted_rollout_item,
};

/// A [`ThreadStore`] that keeps every thread's records in memory, for tests and for hosts that do
/// not persist.
///
/// Clones share the threads. A thread is created with its session metadata as its first record, as
/// Codex's in-memory store records `SessionMeta` on creation, and its records are kept as the
/// rollout directory would write them, numbered from zero and filtered by the same persistence
/// policy. Either way of creating a thread refuses a session the store already holds, as the
/// directory does, where Codex's in-memory store appends to it.
///
/// The recorders it hands back derive the thread's metadata from what is recorded, as Codex's live
/// thread does over its in-memory store, and the store keeps every field of it.
///
/// As in Codex, resuming installs supplied history or preserves the current history, creating an
/// empty history if absent. Persist, shutdown and discard do not invalidate recording handles:
/// there is no live file writer to close. Only the local directory enforces exclusive ownership.
///
/// Its management is Codex's in-memory store's, which serves as a test double: a listing returns
/// every thread, ordered by session id, on one page whatever it was asked; a metadata patch is
/// merged into the patches the thread was given before, and a read applies them over the thread's
/// session metadata; archiving keeps nothing and unarchiving reads the thread; deleting forgets
/// it.
#[derive(Debug, Clone, Default)]
pub struct InMemoryThreadStore {
    threads: Arc<Mutex<HashMap<SessionId, Vec<RolloutRecord>>>>,
    metadata: Arc<Mutex<HashMap<SessionId, ThreadMetadataPatch>>>,
}

impl InMemoryThreadStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn threads(&self) -> MutexGuard<'_, HashMap<SessionId, Vec<RolloutRecord>>> {
        self.threads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn recorder(&self, session_id: &SessionId) -> Arc<dyn RolloutRecorder> {
        Arc::new(InMemoryRecorder {
            threads: Arc::clone(&self.threads),
            session_id: session_id.clone(),
            state: Arc::new(Mutex::new(RecorderState::default())),
        })
    }

    /// The metadata patches the thread of `session_id` was given, merged in the order they were
    /// applied, explicit and derived alike: what Codex's in-memory store keeps as its metadata
    /// updates. A read applies only the fields [`StoredThread`] has; this shows the rest, such as
    /// the derived title.
    #[must_use]
    pub fn thread_metadata(&self, session_id: &SessionId) -> Option<ThreadMetadataPatch> {
        self.metadata().get(session_id).cloned()
    }

    fn metadata(&self) -> MutexGuard<'_, HashMap<SessionId, ThreadMetadataPatch>> {
        self.metadata
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The thread of `session_id` as Codex's in-memory store reads it: its session metadata, with
    /// the patches it was given applied over it.
    fn stored_thread(&self, session_id: &SessionId, include_history: bool) -> Result<StoredThread> {
        let history = StoredThreadHistory::new(session_id.clone(), self.records(session_id)?);
        let mut thread = if include_history {
            StoredThread::from_history(history)?
        } else {
            let meta = first_session_meta(history.records())?;
            StoredThread::new(session_id.clone(), meta.as_ref())
        };
        if let Some(patch) = self.metadata().get(session_id) {
            thread.apply_patch(patch);
        }
        Ok(thread)
    }

    fn records(&self, session_id: &SessionId) -> Result<Vec<RolloutRecord>> {
        self.threads()
            .get(session_id)
            .cloned()
            .ok_or_else(|| not_found(session_id))
    }
}

#[async_trait]
impl RolloutThreadStore for InMemoryThreadStore {
    async fn create_thread(
        &self,
        session_id: &SessionId,
        spawn: &RolloutThreadSpawn,
    ) -> Result<Arc<dyn RolloutRecorder>> {
        let meta = RolloutSessionMeta::new(session_id.clone()).with_thread_spawn(spawn.clone());
        let record = RolloutRecord::new(
            0,
            EventTimestamp::now(),
            RolloutPayload::SessionMeta(meta.clone()),
        )?;
        {
            let mut threads = self.threads();
            // The same conflict check as `create_thread_with`: a created thread is a new one.
            if threads.contains_key(session_id) {
                return Err(thread_exists(session_id));
            }
            threads.insert(session_id.clone(), vec![record]);
        }
        Ok(LiveThreadRecorder::created(
            Arc::new(self.clone()),
            self.recorder(session_id),
            &meta,
            &[],
        ))
    }
}

#[async_trait]
impl ThreadStore for InMemoryThreadStore {
    async fn create_thread_with(
        &self,
        params: &CreateThreadParams,
    ) -> Result<Arc<dyn RolloutRecorder>> {
        let history = initial_payloads(params.history())?;
        let mut payloads = vec![RolloutPayload::SessionMeta(params.meta().clone())];
        payloads.extend(history.iter().cloned());
        let created = (0..)
            .zip(payloads)
            .map(|(timeline_seq, payload)| {
                RolloutRecord::new(timeline_seq, EventTimestamp::now(), payload)
            })
            .collect::<Result<Vec<_>>>()?;
        {
            let mut threads = self.threads();
            if threads.contains_key(params.session_id()) {
                return Err(thread_exists(params.session_id()));
            }
            threads.insert(params.session_id().clone(), created);
        }
        Ok(LiveThreadRecorder::created(
            Arc::new(self.clone()),
            self.recorder(params.session_id()),
            params.meta(),
            &history,
        ))
    }

    async fn resume_thread(&self, params: &ResumeThreadParams) -> Result<Arc<dyn RolloutRecorder>> {
        let history = {
            let mut threads = self.threads();
            if let Some(history) = params.history() {
                threads.insert(params.session_id().clone(), history.to_vec());
            }
            threads
                .entry(params.session_id().clone())
                .or_default()
                .clone()
        };
        Ok(LiveThreadRecorder::resumed(
            Arc::new(self.clone()),
            self.recorder(params.session_id()),
            params.session_id().clone(),
            &history,
        ))
    }

    async fn load_history(&self, params: &LoadThreadHistoryParams) -> Result<StoredThreadHistory> {
        let records = self.records(params.session_id())?;
        Ok(StoredThreadHistory::new(
            params.session_id().clone(),
            records,
        ))
    }

    async fn read_thread(&self, params: &ReadThreadParams) -> Result<StoredThread> {
        self.stored_thread(params.session_id(), params.include_history())
    }

    async fn list_threads(&self, _params: &ListThreadsParams) -> Result<ThreadPage> {
        let mut session_ids = self.threads().keys().cloned().collect::<Vec<_>>();
        session_ids.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        let items = session_ids
            .iter()
            .map(|session_id| self.stored_thread(session_id, false))
            .collect::<Result<Vec<_>>>()?;
        Ok(ThreadPage::new(items, None))
    }

    async fn update_thread_metadata(
        &self,
        params: &UpdateThreadMetadataParams,
    ) -> Result<Option<StoredThread>> {
        let session_id = params.session_id();
        if !self.threads().contains_key(session_id) {
            return Err(not_found(session_id));
        }
        self.metadata()
            .entry(session_id.clone())
            .or_default()
            .merge(params.patch().clone());
        self.stored_thread(session_id, false).map(Some)
    }

    async fn archive_thread(&self, _params: &ArchiveThreadParams) -> Result<()> {
        Ok(())
    }

    async fn unarchive_thread(&self, params: &ArchiveThreadParams) -> Result<StoredThread> {
        self.stored_thread(params.session_id(), false)
    }

    async fn delete_thread(&self, params: &DeleteThreadParams) -> Result<()> {
        let existed = self.threads().remove(params.session_id()).is_some();
        let had_metadata = self.metadata().remove(params.session_id()).is_some();
        if existed || had_metadata {
            Ok(())
        } else {
            Err(not_found(params.session_id()))
        }
    }
}

#[derive(Debug, Default)]
struct RecorderState {
    /// The first record that could not be kept since the last flush.
    failed: Option<Error>,
}

/// Appends a thread's records to the store as they are recorded.
#[derive(Debug)]
struct InMemoryRecorder {
    threads: Arc<Mutex<HashMap<SessionId, Vec<RolloutRecord>>>>,
    session_id: SessionId,
    state: Arc<Mutex<RecorderState>>,
}

impl InMemoryRecorder {
    fn state(&self) -> MutexGuard<'_, RecorderState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn append(&self, item: RolloutItem) -> Result<()> {
        let mut threads = self
            .threads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let records = threads.entry(self.session_id.clone()).or_default();
        let timeline_seq = records.last().map_or(0, |last| last.timeline_seq() + 1);
        records.push(RolloutRecord::new(
            timeline_seq,
            EventTimestamp::now(),
            item.into(),
        )?);
        Ok(())
    }
}

#[async_trait]
impl RolloutRecorder for InMemoryRecorder {
    fn record(&self, item: RolloutItem) {
        if !is_persisted_rollout_item(&item) {
            return;
        }
        let mut state = self.state();
        if let Err(error) = self.append(item) {
            state.failed.get_or_insert(error);
        }
    }

    async fn flush(&self) -> Result<()> {
        let mut state = self.state();
        state.failed.take().map_or(Ok(()), Err)
    }
}

fn not_found(session_id: &SessionId) -> Error {
    Error::session(
        SessionErrorKind::NotFound,
        format!("the store holds no thread of session `{session_id}`"),
    )
}
