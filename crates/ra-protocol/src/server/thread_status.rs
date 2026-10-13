//! What each loaded thread is doing, as clients are told it: Codex's `ThreadWatchManager`
//! (`app-server/src/thread_status.rs`).
//!
//! The listener of a loaded thread reports its turns starting and ending and its errors; from
//! those the manager derives the thread's [`ThreadStatus`], and sends every connection
//! `thread/status/changed` when it changes. A thread the manager has not heard of, or one that
//! has been unloaded, is not loaded.
//!
//! # Differences from Codex
//!
//! Codex's manager also counts the threads running a turn, which its server watches to finish a
//! drain and its listeners watch to unload idle threads. Here those already follow the
//! subscription index's record of active turns (see [`super::thread_state`]), which counts a turn
//! as running until its end has been sent; this manager only reports statuses. An active thread
//! waits on approval while an approval request it sent is unanswered (see
//! [`ThreadWatchManager::note_permission_requested`]), and on the user's input while a question it
//! asked is (see [`ThreadWatchManager::note_user_input_requested`]).

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use ra_core::session::SessionId;

use super::{
    outgoing::{OutgoingMessageSender, ServerNotification},
    protocol::{
        THREAD_STATUS_CHANGED_METHOD, ThreadActiveFlag, ThreadStatus,
        ThreadStatusChangedNotification,
    },
};

/// The facts a status is derived from: Codex's `RuntimeFacts`.
#[derive(Debug, Clone, Default)]
struct RuntimeFacts {
    is_loaded: bool,
    running: bool,
    has_system_error: bool,
    pending_permission_requests: u32,
    pending_user_input_requests: u32,
}

/// A loaded thread's status: Codex's `loaded_thread_status`.
fn loaded_thread_status(runtime: &RuntimeFacts) -> ThreadStatus {
    if !runtime.is_loaded {
        return ThreadStatus::NotLoaded;
    }
    let mut active_flags = Vec::new();
    if runtime.pending_permission_requests > 0 {
        active_flags.push(ThreadActiveFlag::WaitingOnApproval);
    }
    if runtime.pending_user_input_requests > 0 {
        active_flags.push(ThreadActiveFlag::WaitingOnUserInput);
    }
    if runtime.running || !active_flags.is_empty() {
        return ThreadStatus::Active { active_flags };
    }
    if runtime.has_system_error {
        return ThreadStatus::SystemError;
    }
    ThreadStatus::Idle
}

/// Prefers a turn known to be running over a status not yet updated for it: Codex's
/// `resolve_thread_status`.
pub(crate) fn resolve_thread_status(
    status: ThreadStatus,
    has_in_progress_turn: bool,
) -> ThreadStatus {
    if has_in_progress_turn && matches!(status, ThreadStatus::Idle | ThreadStatus::NotLoaded) {
        return ThreadStatus::active();
    }
    status
}

/// The statuses of the loaded threads: Codex's `ThreadWatchManager`.
#[derive(Debug)]
pub(crate) struct ThreadWatchManager {
    runtime_by_thread_id: Mutex<HashMap<SessionId, RuntimeFacts>>,
    outgoing: Arc<OutgoingMessageSender>,
}

impl ThreadWatchManager {
    pub(crate) fn new(outgoing: Arc<OutgoingMessageSender>) -> Self {
        Self {
            runtime_by_thread_id: Mutex::default(),
            outgoing,
        }
    }

    /// Records a thread as loaded, telling every connection if its status changed: Codex's
    /// `upsert_thread`.
    pub(crate) async fn upsert_thread(&self, thread_id: &SessionId) {
        let notification = {
            let mut state = self.lock();
            let previous = status_for(&state, thread_id);
            state.entry(thread_id.clone()).or_default().is_loaded = true;
            changed(&state, thread_id, previous.as_ref())
        };
        self.publish(notification).await;
    }

    /// Records a thread as loaded without telling anyone, as a thread introduced by
    /// `thread/started` is: Codex's `upsert_thread_silently`.
    pub(crate) fn upsert_thread_silently(&self, thread_id: &SessionId) {
        self.lock().entry(thread_id.clone()).or_default().is_loaded = true;
    }

    /// Forgets a thread that was unloaded: Codex's `remove_thread`.
    pub(crate) async fn remove_thread(&self, thread_id: &SessionId) {
        let notification = {
            let mut state = self.lock();
            let previous = status_for(&state, thread_id);
            state.remove(thread_id);
            (previous.is_some() && previous != Some(ThreadStatus::NotLoaded)).then(|| {
                ThreadStatusChangedNotification::new(thread_id.as_str(), ThreadStatus::NotLoaded)
            })
        };
        self.publish(notification).await;
    }

    /// The thread's status, not loaded if it is unknown: Codex's `loaded_status_for_thread`.
    pub(crate) fn loaded_status_for_thread(&self, thread_id: &SessionId) -> ThreadStatus {
        status_for(&self.lock(), thread_id).unwrap_or(ThreadStatus::NotLoaded)
    }

    /// A turn of the thread started: Codex's `note_turn_started`.
    pub(crate) async fn note_turn_started(&self, thread_id: &SessionId) {
        self.update_runtime(thread_id, |runtime| {
            runtime.running = true;
            runtime.has_system_error = false;
        })
        .await;
    }

    /// The thread's turn ended, completed or aborted: Codex's `note_turn_completed` and
    /// `note_turn_interrupted`.
    pub(crate) async fn note_turn_ended(&self, thread_id: &SessionId) {
        self.update_runtime(thread_id, |runtime| {
            runtime.running = false;
            runtime.pending_permission_requests = 0;
            runtime.pending_user_input_requests = 0;
        })
        .await;
    }

    /// The thread's turn hit an error: Codex's `note_system_error`.
    pub(crate) async fn note_system_error(&self, thread_id: &SessionId) {
        self.update_runtime(thread_id, |runtime| {
            runtime.running = false;
            runtime.pending_permission_requests = 0;
            runtime.pending_user_input_requests = 0;
            runtime.has_system_error = true;
        })
        .await;
    }

    /// The thread sent an approval request, which it waits on until the returned guard is dropped:
    /// Codex's `note_permission_requested`.
    pub(crate) async fn note_permission_requested(
        self: &Arc<Self>,
        thread_id: &SessionId,
    ) -> ThreadWatchActiveGuard {
        self.note_pending_request(thread_id, ThreadWatchActiveGuardType::Permission)
            .await
    }

    /// The thread asked the user questions, whose answers it waits on until the returned guard is
    /// dropped: Codex's `note_user_input_requested`.
    pub(crate) async fn note_user_input_requested(
        self: &Arc<Self>,
        thread_id: &SessionId,
    ) -> ThreadWatchActiveGuard {
        self.note_pending_request(thread_id, ThreadWatchActiveGuardType::UserInput)
            .await
    }

    async fn note_pending_request(
        self: &Arc<Self>,
        thread_id: &SessionId,
        guard_type: ThreadWatchActiveGuardType,
    ) -> ThreadWatchActiveGuard {
        self.update_runtime(thread_id, |runtime| {
            let counter = pending_counter(runtime, guard_type);
            *counter = counter.saturating_add(1);
        })
        .await;
        ThreadWatchActiveGuard {
            manager: Arc::clone(self),
            thread_id: thread_id.clone(),
            guard_type,
            handle: tokio::runtime::Handle::current(),
        }
    }

    async fn note_active_guard_released(
        &self,
        thread_id: &SessionId,
        guard_type: ThreadWatchActiveGuardType,
    ) {
        self.update_runtime(thread_id, |runtime| {
            let counter = pending_counter(runtime, guard_type);
            *counter = counter.saturating_sub(1);
        })
        .await;
    }

    async fn update_runtime(&self, thread_id: &SessionId, update: impl FnOnce(&mut RuntimeFacts)) {
        let notification = {
            let mut state = self.lock();
            let previous = status_for(&state, thread_id);
            let runtime = state.entry(thread_id.clone()).or_default();
            runtime.is_loaded = true;
            update(runtime);
            changed(&state, thread_id, previous.as_ref())
        };
        self.publish(notification).await;
    }

    async fn publish(&self, notification: Option<ThreadStatusChangedNotification>) {
        let Some(notification) = notification else {
            return;
        };
        match serde_json::to_value(notification) {
            Ok(params) => {
                self.outgoing
                    .send_server_notification(ServerNotification::new(
                        THREAD_STATUS_CHANGED_METHOD,
                        params,
                    ))
                    .await;
            }
            // The notification is a plain record, so this does not happen.
            Err(error) => {
                tracing::error!("failed to serialize {THREAD_STATUS_CHANGED_METHOD}: {error}");
            }
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<SessionId, RuntimeFacts>> {
        self.runtime_by_thread_id
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

/// What a thread waits on while a guard is held: Codex's `ThreadWatchActiveGuardType`.
#[derive(Debug, Clone, Copy)]
enum ThreadWatchActiveGuardType {
    Permission,
    UserInput,
}

/// The count of the requests of `guard_type` the thread waits on: Codex's `pending_counter`.
const fn pending_counter(
    runtime: &mut RuntimeFacts,
    guard_type: ThreadWatchActiveGuardType,
) -> &mut u32 {
    match guard_type {
        ThreadWatchActiveGuardType::Permission => &mut runtime.pending_permission_requests,
        ThreadWatchActiveGuardType::UserInput => &mut runtime.pending_user_input_requests,
    }
}

/// Keeps its thread waiting on approval or on the user's input until dropped: Codex's
/// `ThreadWatchActiveGuard`.
pub(crate) struct ThreadWatchActiveGuard {
    manager: Arc<ThreadWatchManager>,
    thread_id: SessionId,
    guard_type: ThreadWatchActiveGuardType,
    handle: tokio::runtime::Handle,
}

impl Drop for ThreadWatchActiveGuard {
    fn drop(&mut self) {
        let manager = Arc::clone(&self.manager);
        let thread_id = self.thread_id.clone();
        let guard_type = self.guard_type;
        self.handle.spawn(async move {
            manager
                .note_active_guard_released(&thread_id, guard_type)
                .await;
        });
    }
}

fn status_for(
    state: &HashMap<SessionId, RuntimeFacts>,
    thread_id: &SessionId,
) -> Option<ThreadStatus> {
    state.get(thread_id).map(loaded_thread_status)
}

/// The notification a change from `previous` calls for: Codex's `status_changed_notification`.
fn changed(
    state: &HashMap<SessionId, RuntimeFacts>,
    thread_id: &SessionId,
    previous: Option<&ThreadStatus>,
) -> Option<ThreadStatusChangedNotification> {
    let status = status_for(state, thread_id)?;
    (previous != Some(&status))
        .then(|| ThreadStatusChangedNotification::new(thread_id.as_str(), status))
}
