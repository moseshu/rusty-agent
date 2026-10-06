//! The recorder a store hands back for a live thread: Codex's `LiveThread`
//! (`thread-store/src/live_thread.rs`) as far as it keeps the thread's metadata in step with what
//! is appended to it.
//!
//! Every record is passed to the store's own recorder and observed by the thread's
//! [`ThreadMetadataSync`]; a patch the sync says to write goes to the store through
//! [`ThreadStore::record_thread_metadata`]. Flushing, persisting and shutting down settle the
//! pending patch as Codex's live thread does at those barriers.
//!
//! # Differences from Codex
//!
//! Codex's session awaits each append and then the metadata write it leads to. Recording here
//! cannot wait, so [`RolloutRecorder::record`] returns once the record is handed to the store's
//! recorder and observed, and the write is made by a task of the thread's own. Records are handed
//! over and observed under one lock, so the sync sees them in the order they were recorded. The
//! task makes every write and runs every barrier in the order they were asked for: a patch is never
//! written after a later one, and a barrier waits for every write asked for before it. A write
//! that fails is logged and its patch stays pending, as Codex's session logs a failed append; the
//! next write or barrier tries again, and a barrier reports how its own attempt went.
//!
//! Codex's barriers write with `update_thread_metadata` and its appends with
//! `record_thread_metadata`; here both go through `record_thread_metadata`, which both stores that
//! use this recorder apply before returning. Discarding the thread also forgets its pending patch.

use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
use ra_core::{
    error::{Error, Result, SessionErrorKind},
    event::EventTimestamp,
    session::{
        SessionId,
        rollout::{RolloutItem, RolloutRecorder},
    },
};
use tokio::sync::{mpsc, oneshot};

use super::{
    ThreadStore, UpdateThreadMetadataParams,
    metadata_sync::{Observed, PendingThreadMetadataPatch, ThreadMetadataSync, readable_payloads},
};
use crate::rollout::{
    RolloutPayload, RolloutRecord, RolloutSessionMeta, recorder::is_persisted_rollout_item,
};

/// What the thread's task is asked to do, in order.
enum Command {
    /// Write an eligible observation's submission request, if one is still pending.
    Commit,
    Flush(oneshot::Sender<Result<()>>),
    Persist(oneshot::Sender<Result<()>>),
    Shutdown(oneshot::Sender<Result<()>>),
    Discard(oneshot::Sender<Result<()>>),
}

/// A store's recorder for a live thread, with the thread's metadata kept in step.
pub(crate) struct LiveThreadRecorder {
    inner: Arc<dyn RolloutRecorder>,
    sync: Arc<Mutex<ThreadMetadataSync>>,
    commands: mpsc::UnboundedSender<Command>,
}

impl LiveThreadRecorder {
    /// Wraps `inner`, the store's recorder for the thread of `session_id`, writing what `sync`
    /// derives to `store`. With `commit`, what `sync` already holds is written at once, as Codex's
    /// session writes what a forked history appended at creation says.
    ///
    /// Must be called within a Tokio runtime, which the thread's task needs.
    fn start(
        session_id: SessionId,
        inner: Arc<dyn RolloutRecorder>,
        store: Arc<dyn ThreadStore>,
        sync: ThreadMetadataSync,
        commit: bool,
    ) -> Arc<dyn RolloutRecorder> {
        let sync = Arc::new(Mutex::new(sync));
        let (commands, receiver) = mpsc::unbounded_channel();
        let task = ThreadTask {
            session_id,
            inner: Arc::clone(&inner),
            store,
            sync: Arc::clone(&sync),
        };
        tokio::spawn(task.run(receiver));
        if commit {
            let _ = commands.send(Command::Commit);
        }
        Arc::new(Self {
            inner,
            sync,
            commands,
        })
    }

    /// The recorder of a thread just created with `meta` and the `history` it starts with, already
    /// filtered by the persistence policy, which is observed as Codex's session appends a forked
    /// history before anything else.
    pub(crate) fn created(
        store: Arc<dyn ThreadStore>,
        inner: Arc<dyn RolloutRecorder>,
        meta: &RolloutSessionMeta,
        history: &[RolloutPayload],
    ) -> Arc<dyn RolloutRecorder> {
        let mut sync = ThreadMetadataSync::for_create(meta);
        let at = EventTimestamp::now();
        let observed = history
            .iter()
            .map(|payload| Observed::of_payload(payload, at))
            .collect::<Vec<_>>();
        let commit = sync.observe_appended_items(&observed).is_some();
        Self::start(meta.session_id().clone(), inner, store, sync, commit)
    }

    /// The recorder of a thread of `session_id` just resumed with `history`.
    pub(crate) fn resumed(
        store: Arc<dyn ThreadStore>,
        inner: Arc<dyn RolloutRecorder>,
        session_id: SessionId,
        history: &[RolloutRecord],
    ) -> Arc<dyn RolloutRecorder> {
        let payloads = readable_payloads(history);
        let observed = payloads
            .iter()
            .map(|(payload, at)| Observed::of_payload(payload, *at))
            .collect::<Vec<_>>();
        let sync = ThreadMetadataSync::for_resume(session_id.clone(), &observed);
        Self::start(session_id, inner, store, sync, false)
    }

    async fn barrier(
        &self,
        command: impl FnOnce(oneshot::Sender<Result<()>>) -> Command,
    ) -> Result<()> {
        let (done, finished) = oneshot::channel();
        self.commands.send(command(done)).map_err(|_| stopped())?;
        finished.await.map_err(|_| stopped())?
    }
}

impl std::fmt::Debug for LiveThreadRecorder {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LiveThreadRecorder")
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl RolloutRecorder for LiveThreadRecorder {
    fn record(&self, item: RolloutItem) {
        // Codex observes only what its persistence policy keeps.
        if !is_persisted_rollout_item(&item) {
            self.inner.record(item);
            return;
        }
        let mut sync = lock(&self.sync);
        let update = sync.observe_appended_items(&[Observed::of_item(&item)]);
        self.inner.record(item);
        if update.is_some() {
            // The task outlives every sender, so this fails only if it panicked; the next barrier
            // reports that.
            let _ = self.commands.send(Command::Commit);
        }
    }

    async fn flush(&self) -> Result<()> {
        self.barrier(Command::Flush).await
    }

    async fn persist(&self) -> Result<()> {
        self.barrier(Command::Persist).await
    }

    async fn shutdown(&self) -> Result<()> {
        self.barrier(Command::Shutdown).await
    }

    async fn discard(&self) -> Result<()> {
        self.barrier(Command::Discard).await
    }
}

/// What the thread's task owns.
struct ThreadTask {
    session_id: SessionId,
    inner: Arc<dyn RolloutRecorder>,
    store: Arc<dyn ThreadStore>,
    sync: Arc<Mutex<ThreadMetadataSync>>,
}

impl ThreadTask {
    async fn run(self, mut commands: mpsc::UnboundedReceiver<Command>) {
        while let Some(command) = commands.recv().await {
            match command {
                Command::Commit => {
                    let update = lock(&self.sync).take_pending_update_for_commit();
                    if let Err(error) = self.write(update).await {
                        tracing::warn!(
                            session_id = %self.session_id,
                            %error,
                            "failed to record thread metadata; it stays pending"
                        );
                    }
                }
                Command::Flush(done) => {
                    let _ = done.send(self.flush().await);
                }
                Command::Persist(done) => {
                    let _ = done.send(self.persist().await);
                }
                Command::Shutdown(done) => {
                    let _ = done.send(self.shutdown().await);
                }
                Command::Discard(done) => {
                    lock(&self.sync).discard_pending_update();
                    let _ = done.send(self.inner.discard().await);
                }
            }
        }
    }

    /// Codex's `LiveThread::flush`: the records, then what is pending for a thread with history.
    async fn flush(&self) -> Result<()> {
        self.inner.flush().await?;
        let update = lock(&self.sync).take_pending_update_for_existing_history();
        self.write(update).await
    }

    /// Codex's `LiveThread::persist` in its standard context: the thread is made durable, so
    /// whatever is pending is written, a created thread's initial metadata included.
    async fn persist(&self) -> Result<()> {
        self.inner.persist().await?;
        let update = lock(&self.sync).take_pending_update();
        self.write(update).await
    }

    /// Codex's `LiveThread::shutdown`: what is pending for a thread with history, then the
    /// recorder, reporting both failures if both fail.
    async fn shutdown(&self) -> Result<()> {
        let update = lock(&self.sync).take_pending_update_for_existing_history();
        let metadata = self.write(update).await;
        let shutdown = self.inner.shutdown().await;
        match (metadata, shutdown) {
            (Err(metadata), Err(shutdown)) => Err(Error::session(
                SessionErrorKind::Io,
                format!(
                    "thread metadata update failed: {metadata}; thread shutdown failed: {shutdown}"
                ),
            )),
            (Err(metadata), Ok(())) => Err(metadata),
            (Ok(()), result) => result,
        }
    }

    /// Codex's `apply_pending_metadata_update`: writes `update` and marks it applied.
    async fn write(&self, update: Option<PendingThreadMetadataPatch>) -> Result<()> {
        let Some(update) = update else {
            return Ok(());
        };
        let params = UpdateThreadMetadataParams::new(self.session_id.clone(), update.patch.clone())
            .including_archived();
        self.store.record_thread_metadata(&params).await?;
        lock(&self.sync).mark_pending_update_applied(&update);
        Ok(())
    }
}

fn lock(sync: &Mutex<ThreadMetadataSync>) -> MutexGuard<'_, ThreadMetadataSync> {
    sync.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn stopped() -> Error {
    Error::session(
        SessionErrorKind::Io,
        "the live thread's metadata task has stopped",
    )
}
