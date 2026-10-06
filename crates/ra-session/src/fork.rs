//! Forking a thread: a new thread that starts from a snapshot of another's history.
//!
//! Ported from Codex's `ThreadManager::fork_thread_from_history` and `fork_legacy_thread`
//! (`core/src/thread_manager.rs`) and from what its session does with a forked history when it
//! starts (`core/src/session/mod.rs`). A fork always gets a new thread with a session id of its
//! own. The source's records are cut as the [`ForkSnapshot`] says and copied into the new thread's
//! rollout, after session metadata that names the source as `forked_from_id`; the new rollout is
//! materialized at once, and the history is rebuilt from the copy. The source is left as it was.
//!
//! As with [`ResumedThread`](crate::ResumedThread), the run half is the host's: the next run
//! starts on [`ForkedThread::reconstruction`]'s history followed by its new input, records through
//! [`ForkedThread::recorder`], and is told how much of its input the rollout already holds
//! (`RunRequest::with_recorded_input`, with the history's length).
//!
//! # Differences from Codex
//!
//! - Codex's turn is a run here, and its turn lifecycle is the run's: a run whose latest segment
//!   recorded no end, or ended paused for approval, is still in progress, as Codex's turn is while
//!   it waits for an approval. Interrupting it appends what a cancelled run records: the
//!   interrupted-run marker, unless it is disabled, then [`RolloutRunEnd::Cancelled`] — Codex's
//!   `<turn_aborted>` message and `TurnAborted`. Codex picks the marker from the new thread's
//!   configuration and multi-agent version; there is no such configuration here, so the caller
//!   picks it with [`ForkThreadParams::with_interrupted_turn_marker`], defaulting to Codex's
//!   default outside multi-agent v2.
//! - A fork's session id must be new to the store. Codex always generates one; a caller here can
//!   choose it, and a store refuses one it already holds before writing anything.
//! - The copy leaves out what belongs to the source's file rather than to its history: the
//!   checkpoints its writer wrote, which summarize that file. The source's session metadata is
//!   copied, as Codex copies it; only the first record describes the thread.
//! - Codex also appends the settings the new thread applied, inherits the source's
//!   instructions, multi-agent version and token usage display, and reports the start to session
//!   hooks as a fork. Those belong to its session; a host here reads the source's last turn
//!   context from the reconstruction if it wants it. Copying the source's attachments and the
//!   reference-backed forks of Codex's paginated history mode are not ported.
//! - A live source's recorder may hold records it has not written yet. Codex flushes a live
//!   source before it snapshots it for a spawned agent; [`ForkedThread::fork`] reads what the
//!   store holds, so a host forking a thread it is running flushes the thread's recorder first.

use std::sync::Arc;

use ra_core::{
    error::Result,
    event::EventTimestamp,
    session::{
        InterruptedTurnHistoryMarker, SessionId,
        rollout::{PersistContext, RolloutRecorder, RolloutRunEnd, RolloutRunEnded},
    },
    state::RunId,
};

use crate::{
    rollout::{
        RolloutPayload, RolloutReconstruction, RolloutRecord, RolloutSessionMeta,
        reconstruct_history, truncate_rollout_before_nth_user_message,
        user_message_positions_in_rollout,
    },
    store::{CreateThreadParams, LoadThreadHistoryParams, StoredThreadHistory, ThreadStore},
};

/// Which part of the source's history a fork starts from: Codex's `ForkSnapshot`.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkSnapshot {
    /// The history strictly before the `n`th user message, counted from zero.
    ///
    /// When the source holds fewer user messages and its newest run is still in progress, the
    /// cut falls before that run's first segment instead, so the fork leaves the unfinished run
    /// out; when no run is in progress, the whole history is kept. User messages are those
    /// [`user_message_positions_in_rollout`] finds.
    TruncateBeforeNthUserMessage(usize),
    /// The history as it stands, as if the source had been interrupted now.
    ///
    /// When the newest run is still in progress, the fork's copy ends with what a cancelled run
    /// records — the interrupted-run marker, then the run's end — otherwise the history is copied
    /// as it stands.
    Interrupted,
}

/// A count of user messages is a truncation, as Codex keeps `fork_legacy_thread(usize, ...)`
/// callers working.
impl From<usize> for ForkSnapshot {
    fn from(n: usize) -> Self {
        Self::TruncateBeforeNthUserMessage(n)
    }
}

/// How a fork is made: the snapshot of the source, the new thread's session metadata, and the
/// marker an interrupted snapshot records.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct ForkThreadParams {
    snapshot: ForkSnapshot,
    session_meta: Option<RolloutSessionMeta>,
    interrupted_turn_marker: InterruptedTurnHistoryMarker,
}

impl ForkThreadParams {
    /// Forks from `snapshot`, into a thread with a generated session id and no other metadata.
    #[must_use]
    pub fn new(snapshot: impl Into<ForkSnapshot>) -> Self {
        Self {
            snapshot: snapshot.into(),
            session_meta: None,
            interrupted_turn_marker: InterruptedTurnHistoryMarker::default(),
        }
    }

    /// Sets the new thread's session metadata; its session id becomes the fork's, as Codex lets a
    /// caller reserve the new thread's id. The id must be new to the store. The fork sets its
    /// `forked_from_id`.
    #[must_use]
    pub fn with_session_meta(mut self, meta: RolloutSessionMeta) -> Self {
        self.session_meta = Some(meta);
        self
    }

    /// Sets the marker an [`ForkSnapshot::Interrupted`] fork of a run in progress records, as the
    /// new thread's configuration picks it in Codex; the default is the user-context form, Codex's
    /// default outside multi-agent v2.
    #[must_use]
    pub const fn with_interrupted_turn_marker(
        mut self,
        marker: InterruptedTurnHistoryMarker,
    ) -> Self {
        self.interrupted_turn_marker = marker;
        self
    }

    /// The snapshot of the source the fork starts from.
    #[must_use]
    pub const fn snapshot(&self) -> ForkSnapshot {
        self.snapshot
    }

    /// The new thread's session metadata, if set.
    #[must_use]
    pub const fn session_meta(&self) -> Option<&RolloutSessionMeta> {
        self.session_meta.as_ref()
    }

    /// The marker an interrupted snapshot records.
    #[must_use]
    pub const fn interrupted_turn_marker(&self) -> InterruptedTurnHistoryMarker {
        self.interrupted_turn_marker
    }
}

/// A thread forked from another, with the history it starts from.
#[non_exhaustive]
pub struct ForkedThread {
    session_id: SessionId,
    forked_from_id: SessionId,
    recorder: Arc<dyn RolloutRecorder>,
    reconstruction: RolloutReconstruction,
}

impl ForkedThread {
    /// Forks the thread of `source` from the history `store` holds for it: Codex's
    /// `fork_legacy_thread`, which reads the source's rollout through its store.
    ///
    /// # Errors
    ///
    /// Returns an error of kind [`SessionErrorKind::NotFound`](ra_core::error::SessionErrorKind) if
    /// the store holds no such thread, or any error [`Self::fork_from_history`] returns.
    pub async fn fork<S>(store: &S, source: &SessionId, params: &ForkThreadParams) -> Result<Self>
    where
        S: ThreadStore + ?Sized,
    {
        let history = store
            .load_history(&LoadThreadHistoryParams::new(source.clone()))
            .await?;
        Self::fork_from_history(store, history, params).await
    }

    /// Forks the thread whose records `history` holds: Codex's `fork_thread_from_history`.
    ///
    /// The snapshot is cut and its history rebuilt before the new thread is created, so a source
    /// that cannot be read creates nothing. The new rollout is then materialized; as in Codex, a
    /// failure to do so is logged rather than failing the fork, and the recorder keeps the copy
    /// queued for its next write.
    ///
    /// # Errors
    ///
    /// Returns an error if the source's records cannot be read or its history cannot be rebuilt
    /// from them, or if the new thread cannot be created.
    pub async fn fork_from_history<S>(
        store: &S,
        history: StoredThreadHistory,
        params: &ForkThreadParams,
    ) -> Result<Self>
    where
        S: ThreadStore + ?Sized,
    {
        let forked_from_id = history.session_id().clone();
        let records = fork_records(params, history.into_records())?;
        let reconstruction = reconstruct_history(&records)?;
        let meta = params
            .session_meta
            .clone()
            .unwrap_or_else(|| RolloutSessionMeta::new(SessionId::generate()))
            .with_forked_from_id(forked_from_id.clone());
        let session_id = meta.session_id().clone();
        let recorder = store
            .create_thread_with(&CreateThreadParams::new(meta).with_history(records))
            .await?;
        if let Err(error) = recorder.persist(PersistContext::Standard).await {
            tracing::warn!(
                session_id = %session_id,
                %error,
                "failed to materialize a forked thread; its history stays queued"
            );
        }
        Ok(Self {
            session_id,
            forked_from_id,
            recorder,
            reconstruction,
        })
    }

    /// The new thread's session.
    #[must_use]
    pub const fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// The session of the thread it was forked from.
    #[must_use]
    pub const fn forked_from_id(&self) -> &SessionId {
        &self.forked_from_id
    }

    /// The recorder the new thread's runs record through.
    #[must_use]
    pub const fn recorder(&self) -> &Arc<dyn RolloutRecorder> {
        &self.recorder
    }

    /// The history rebuilt from the copy, and how its newest run ended.
    #[must_use]
    pub const fn reconstruction(&self) -> &RolloutReconstruction {
        &self.reconstruction
    }

    /// The recorder and the rebuilt history, taken.
    #[must_use]
    pub fn into_parts(self) -> (Arc<dyn RolloutRecorder>, RolloutReconstruction) {
        (self.recorder, self.reconstruction)
    }
}

impl std::fmt::Debug for ForkedThread {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ForkedThread")
            .field("session_id", &self.session_id)
            .field("forked_from_id", &self.forked_from_id)
            .field("reconstruction", &self.reconstruction)
            .finish_non_exhaustive()
    }
}

/// The records a fork copies under its snapshot: Codex's `fork_history_from_snapshot`.
fn fork_records(
    params: &ForkThreadParams,
    mut records: Vec<RolloutRecord>,
) -> Result<Vec<RolloutRecord>> {
    let active = active_run(&records)?;
    match params.snapshot {
        ForkSnapshot::TruncateBeforeNthUserMessage(n) => match active {
            Some(active) if n >= user_message_positions_in_rollout(&records)?.len() => {
                records.truncate(active.start);
                Ok(records)
            }
            _ => truncate_rollout_before_nth_user_message(records, n),
        },
        ForkSnapshot::Interrupted => {
            if let Some(active) = active {
                let marker = params
                    .interrupted_turn_marker
                    .item()
                    .map(RolloutPayload::Item);
                let ended = RolloutRunEnded::new(active.run_id, RolloutRunEnd::Cancelled);
                for payload in marker.into_iter().chain([RolloutPayload::RunEnded(ended)]) {
                    let timeline_seq = records.last().map_or(0, |last| last.timeline_seq() + 1);
                    records.push(RolloutRecord::new(
                        timeline_seq,
                        EventTimestamp::now(),
                        payload,
                    )?);
                }
            }
            Ok(records)
        }
    }
}

/// The newest run, while it is in progress.
struct ActiveRun {
    run_id: RunId,
    /// Where its first segment started.
    start: usize,
}

/// The newest run if it is still in progress: Codex's `snapshot_turn_state`.
///
/// A run is in progress when its latest segment recorded no end, or ended paused for approval.
fn active_run(records: &[RolloutRecord]) -> Result<Option<ActiveRun>> {
    let mut newest: Option<ActiveRun> = None;
    let mut in_progress = false;
    for (index, record) in records.iter().enumerate() {
        match record.type_name() {
            "run_started" => {
                if let RolloutPayload::RunStarted(started) = record.payload()? {
                    if newest
                        .as_ref()
                        .is_none_or(|run| run.run_id != *started.run_id())
                    {
                        newest = Some(ActiveRun {
                            run_id: started.run_id().clone(),
                            start: index,
                        });
                    }
                    in_progress = true;
                }
            }
            "run_ended" => {
                if let RolloutPayload::RunEnded(ended) = record.payload()?
                    && newest
                        .as_ref()
                        .is_some_and(|run| run.run_id == *ended.run_id())
                {
                    in_progress = ended.end() == RolloutRunEnd::Interrupted;
                }
            }
            _ => {}
        }
    }
    Ok(newest.filter(|_| in_progress))
}
