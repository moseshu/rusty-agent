//! Routes spawned agents' checkpoints over the owning thread's client connections.

use ra_runtime::agent::control::AgentControl;
use std::{collections::HashSet, sync::Arc};
use tokio::sync::watch;

use super::{
    approval_requests::start_spawned_approval_batch,
    outgoing::{OutgoingMessageSender, ThreadScopedOutgoingMessageSender},
    thread_manager::LoadedThread,
    thread_status::ThreadWatchManager,
};
use crate::transport::ConnectionId;

pub(crate) struct SpawnedApprovals {
    control: AgentControl,
    changes: watch::Receiver<u64>,
    batches: HashSet<(String, Vec<String>)>,
}

impl SpawnedApprovals {
    pub(crate) fn new(loaded: &LoadedThread) -> Option<Self> {
        let control = loaded.thread.agent_control()?.clone();
        let mut changes = control.subscribe_paused_runs();
        changes.mark_changed();
        Some(Self {
            control,
            changes,
            batches: HashSet::new(),
        })
    }

    pub(crate) fn start_batches(
        &mut self,
        loaded: &LoadedThread,
        outgoing: &Arc<OutgoingMessageSender>,
        watch: &Arc<ThreadWatchManager>,
    ) {
        let previous = std::mem::take(&mut self.batches);
        for paused in self.control.paused_runs() {
            let key = (
                paused.state().run_id().as_str().to_owned(),
                paused
                    .state()
                    .pending_interruption_items()
                    .map(|item| item.id().as_str().to_owned())
                    .collect(),
            );
            if !previous.contains(&key) {
                let connections = outgoing
                    .thread_state()
                    .subscribed_connection_ids(loaded.thread_id());
                let sender = ThreadScopedOutgoingMessageSender::new(
                    outgoing.clone(),
                    connections,
                    loaded.thread_id().clone(),
                );
                start_spawned_approval_batch(
                    loaded.clone(),
                    sender,
                    watch.clone(),
                    self.control.clone(),
                    paused,
                );
            }
            self.batches.insert(key);
        }
    }
}

pub(crate) async fn changed(approvals: &mut Option<SpawnedApprovals>) -> bool {
    match approvals {
        Some(approvals) => approvals.changes.changed().await.is_ok(),
        None => std::future::pending().await,
    }
}

/// A connection resuming the parent also receives the requests its spawned agents still wait on:
/// those of runs paused for approval, and those a tool asks for while its run goes on — a managed
/// network approval or a permission request, which keep no paused run to find them by.
pub(crate) async fn replay(
    loaded: &LoadedThread,
    outgoing: &OutgoingMessageSender,
    connection: ConnectionId,
) {
    if let Some(control) = loaded.thread.agent_control() {
        let mut sessions = Vec::new();
        let agents = control
            .paused_runs()
            .into_iter()
            .map(|paused| paused.session_id().clone())
            .chain(
                control
                    .live_instances()
                    .into_iter()
                    .map(|agent| agent.session_id().clone()),
            );
        for session in agents {
            if !sessions.contains(&session) {
                sessions.push(session);
            }
        }
        for session in &sessions {
            outgoing
                .replay_requests_to_connection_for_thread(connection, session)
                .await;
        }
    }
}
