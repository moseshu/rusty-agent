//! One approval batch belongs to the parent thread or a spawned agent's own paused run.

use ra_core::{
    agent::control::AgentControlPort,
    event::ApprovalEvent,
    item::{ItemId, RunItem},
    state::RunState,
};
use ra_runtime::{
    agent::control::{AgentControl, PausedAgentRun},
    runner::RunConfig,
};
use std::collections::HashSet;

use crate::server::thread_manager::LoadedThread;

pub(super) enum ApprovalTarget {
    Parent,
    Spawned {
        control: AgentControl,
        paused: Box<PausedAgentRun>,
    },
}

impl ApprovalTarget {
    pub(super) fn checkpoint(
        &self,
        loaded: &LoadedThread,
        turn_id: &str,
    ) -> Option<(RunState, RunConfig)> {
        match self {
            Self::Parent => loaded.thread.paused_run_with_config(turn_id),
            Self::Spawned { control, paused } => control
                .paused_runs()
                .into_iter()
                .find(|current| {
                    current.path() == paused.path() && current.state().run_id().as_str() == turn_id
                })
                .map(|current| (current.state().clone(), current.config().clone())),
        }
    }

    pub(super) fn current_batch(
        &self,
        loaded: &LoadedThread,
        turn_id: &str,
        batch: &[RunItem],
    ) -> bool {
        let Some((state, _)) = self.checkpoint(loaded, turn_id) else {
            return false;
        };
        let asked: HashSet<&ItemId> = batch.iter().map(RunItem::id).collect();
        let waiting: HashSet<&ItemId> = state
            .pending_interruption_items()
            .map(RunItem::id)
            .collect();
        asked == waiting
    }

    pub(super) fn record(
        &self,
        loaded: &LoadedThread,
        state: &mut RunState,
        event: ApprovalEvent,
    ) -> ra_core::error::Result<()> {
        match self {
            Self::Parent => loaded.thread.record_approval(state, event),
            Self::Spawned { control, paused } => {
                control.record_approval(paused.path(), state, event)
            }
        }
    }

    pub(super) fn resume(&self, loaded: &LoadedThread, state: RunState) -> bool {
        match self {
            Self::Parent => loaded.thread.resume_turn(state).is_ok(),
            Self::Spawned { control, paused } => control.resume(paused.path(), state).is_ok(),
        }
    }

    pub(super) async fn interrupt(&self, loaded: &LoadedThread) {
        match self {
            Self::Parent => {
                let _ = loaded.thread.interrupt();
            }
            Self::Spawned { control, paused } => {
                let _ = control.root().interrupt(paused.path().as_str()).await;
            }
        }
    }

    /// Whether the batch withdraws its own requests once it ends. The parent's are withdrawn by
    /// its listener at the turn transition, as before spawned agents had batches.
    pub(super) const fn withdraws_own_requests(&self) -> bool {
        matches!(self, Self::Spawned { .. })
    }

    /// A child's cancellation withdraws only that child's requests; parent turn transitions do
    /// not end the child's run. Parent requests retain their existing listener withdrawal path.
    pub(super) async fn changed(&self, loaded: &LoadedThread, turn_id: &str, batch: &[RunItem]) {
        let Self::Spawned { control, .. } = self else {
            return std::future::pending().await;
        };
        let mut changes = control.subscribe_paused_runs();
        while self.current_batch(loaded, turn_id, batch) {
            if changes.changed().await.is_err() {
                break;
            }
        }
    }
}
