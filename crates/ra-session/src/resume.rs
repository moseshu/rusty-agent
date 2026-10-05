//! Resuming a thread from what its store holds.
//!
//! Ported from Codex's `ThreadManager::resume_legacy_thread_from_rollout`
//! (`core/src/thread_manager.rs`), which reads a thread's whole rollout and starts the thread again
//! on it, and from what its session does with that history when it starts
//! (`core/src/session/mod.rs`): rebuild the model-visible history and reopen the live writer, so
//! the next turn is appended to the same rollout and starts from that history.
//!
//! [`ResumedThread::resume`] does the storage half: it reopens the thread, then loads its records
//! from a [`ThreadStore`] and rebuilds its history with [`reconstruct_history`]. Local ownership
//! prevents another writer from changing the history between loading and handoff. The run half
//! is the host's, since a run here is started by its host rather than
//! by a live thread object:
//!
//! - When [`RolloutReconstruction::last_run`] ended interrupted, waiting for approval, the run is
//!   continued from the checkpoint the host kept of it (`RunRequest::with_state`), as before the
//!   resume.
//! - Otherwise the next run starts on [`RolloutReconstruction::history`] followed by its new input,
//!   is given the recorder (`RunRequest::with_rollout_recorder`), and is told how much of its input
//!   the rollout already holds (`RunRequest::with_recorded_input`, with the history's length), so
//!   only the new input is recorded.
//!
//! # Differences from Codex
//!
//! As Codex does on failed startup, a reopened writer is discarded if history loading or
//! reconstruction fails. Unlike its thread manager's preliminary history read, the local
//! snapshot is read after acquiring writer ownership, so a previous writer's last records cannot
//! be omitted. Dropping the resume future drops its recorder and lets its writer task close.
//!
//! Codex's session also warns when the thread resumes on a different model than it last ran on,
//! seeds the token usage it shows from the rollout, and records the settings the resumed thread
//! applies. Those are its product's; a host here reads the last turn context and the usage totals
//! from the records if it wants them.
//!
//! The agents a thread spawned are not resumed with it. Codex restores them from its agent graph
//! store, listing the spawn edges still open below the thread, and reloads each through its loaded
//! parent; that store is not ported yet.

use std::sync::Arc;

use ra_core::{error::Result, session::rollout::RolloutRecorder};

use crate::{
    rollout::{RolloutReconstruction, reconstruct_history},
    store::{LoadThreadHistoryParams, ResumeThreadParams, StoredThreadHistory, ThreadStore},
};

/// A thread reopened for more records, with the history it continues from.
#[non_exhaustive]
pub struct ResumedThread {
    recorder: Arc<dyn RolloutRecorder>,
    history: StoredThreadHistory,
    reconstruction: RolloutReconstruction,
}

impl ResumedThread {
    /// Reopens the thread `params` names, then loads and rebuilds its history under local
    /// ownership.
    ///
    /// # Errors
    ///
    /// Returns an error of kind [`SessionErrorKind::NotFound`](ra_core::error::SessionErrorKind) if
    /// a local store holds no such thread, an error if its records cannot be read or its history
    /// cannot be rebuilt from them, or an error if it cannot be reopened — for one thing, because a
    /// live writer still holds it.
    pub async fn resume<S>(store: &S, params: &ResumeThreadParams) -> Result<Self>
    where
        S: ThreadStore + ?Sized,
    {
        let recorder = store.resume_thread(params).await?;
        let loaded = async {
            let history = store
                .load_history(&LoadThreadHistoryParams::new(params.session_id().clone()))
                .await?;
            let reconstruction = reconstruct_history(history.records())?;
            Ok::<_, ra_core::error::Error>((history, reconstruction))
        }
        .await;
        let (history, reconstruction) = match loaded {
            Ok(loaded) => loaded,
            Err(error) => {
                if let Err(discard_error) = recorder.discard().await {
                    tracing::warn!(
                        %discard_error,
                        "failed to discard a thread after resume failed"
                    );
                }
                return Err(error);
            }
        };
        Ok(Self {
            recorder,
            history,
            reconstruction,
        })
    }

    /// The recorder the thread's next runs record through.
    #[must_use]
    pub const fn recorder(&self) -> &Arc<dyn RolloutRecorder> {
        &self.recorder
    }

    /// The thread's records as they were when it was reopened.
    #[must_use]
    pub const fn history(&self) -> &StoredThreadHistory {
        &self.history
    }

    /// The thread's history rebuilt from its records, and how its newest run ended.
    #[must_use]
    pub const fn reconstruction(&self) -> &RolloutReconstruction {
        &self.reconstruction
    }

    /// The recorder, the records and the rebuilt history, taken.
    #[must_use]
    pub fn into_parts(
        self,
    ) -> (
        Arc<dyn RolloutRecorder>,
        StoredThreadHistory,
        RolloutReconstruction,
    ) {
        (self.recorder, self.history, self.reconstruction)
    }
}

impl std::fmt::Debug for ResumedThread {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ResumedThread")
            .field("history", &self.history)
            .field("reconstruction", &self.reconstruction)
            .finish_non_exhaustive()
    }
}
