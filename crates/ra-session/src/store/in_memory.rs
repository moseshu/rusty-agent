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
    LoadThreadHistoryParams, ReadThreadParams, ResumeThreadParams, StoredThread,
    StoredThreadHistory, ThreadStore, first_session_meta,
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
/// policy.
///
/// As in Codex, resuming installs supplied history or preserves the current history, creating an
/// empty history if absent. Persist, shutdown and discard do not invalidate recording handles:
/// there is no live file writer to close. Only the local directory enforces exclusive ownership.
#[derive(Debug, Clone, Default)]
pub struct InMemoryThreadStore {
    threads: Arc<Mutex<HashMap<SessionId, Vec<RolloutRecord>>>>,
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
        {
            let mut threads = self.threads();
            let records = threads.entry(session_id.clone()).or_default();
            records.push(RolloutRecord::new(
                records.last().map_or(0, |last| last.timeline_seq() + 1),
                EventTimestamp::now(),
                RolloutPayload::SessionMeta(meta),
            )?);
        }
        Ok(self.recorder(session_id))
    }
}

#[async_trait]
impl ThreadStore for InMemoryThreadStore {
    async fn resume_thread(&self, params: &ResumeThreadParams) -> Result<Arc<dyn RolloutRecorder>> {
        {
            let mut threads = self.threads();
            if let Some(history) = params.history() {
                threads.insert(params.session_id().clone(), history.to_vec());
            } else {
                threads.entry(params.session_id().clone()).or_default();
            }
        }
        Ok(self.recorder(params.session_id()))
    }

    async fn load_history(&self, params: &LoadThreadHistoryParams) -> Result<StoredThreadHistory> {
        let records = self.records(params.session_id())?;
        Ok(StoredThreadHistory::new(
            params.session_id().clone(),
            records,
        ))
    }

    async fn read_thread(&self, params: &ReadThreadParams) -> Result<StoredThread> {
        let records = self.records(params.session_id())?;
        let history = StoredThreadHistory::new(params.session_id().clone(), records);
        if params.include_history() {
            return StoredThread::from_history(history);
        }
        let meta = first_session_meta(history.records())?;
        Ok(StoredThread::new(
            params.session_id().clone(),
            meta.as_ref(),
        ))
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
