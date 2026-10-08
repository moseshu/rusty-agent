//! The task that reads a loaded thread's events and tells the thread's connections what happened:
//! Codex's thread listener (`request_processors/thread_lifecycle.rs`,
//! `ensure_listener_task_running`) and its handling of turn events
//! (`bespoke_event_handling.rs`).
//!
//! Each event is first tracked in the thread's state, then handled with the connections subscribed
//! at that moment. A turn that starts withdraws the thread's server requests still waiting, as one
//! that ends does, since every request of a thread belongs to a turn; the end of a turn also
//! answers the `turn/interrupt` requests waiting for it.
//!
//! # Differences from Codex
//!
//! Codex's `turn/completed` carries the turn's last final answer as a summary item; items are
//! Codex's `ThreadItem`s, which come with the item notifications, so the turn is sent with no
//! items loaded until then. Codex's listener also handles commands that order a running thread's
//! resume and its goals with its notifications, and stops when the thread is unloaded; those come
//! with the thread methods.

use std::sync::Arc;

use ra_core::session::SessionId;
use ra_runtime::thread::{AgentThread, ThreadEvent};
use serde::Serialize;
use tokio::sync::Mutex;

use super::{
    outgoing::{OutgoingMessageSender, ServerNotification, ThreadScopedOutgoingMessageSender},
    protocol::{
        ERROR_METHOD, ErrorNotification, THREAD_SETTINGS_UPDATED_METHOD, TURN_COMPLETED_METHOD,
        TURN_STARTED_METHOD, ThreadSettingsUpdatedNotification, Turn, TurnCompletedNotification,
        TurnError, TurnInterruptResponse, TurnStartedNotification, TurnStatus,
    },
    settings_adapter::{ThreadSettingsAdapter, settings_snapshot},
    thread_state::ThreadState,
};

/// Starts the thread's listener unless one is already running: Codex's
/// `ensure_listener_task_running`.
pub(crate) async fn ensure_listener_task_running(
    outgoing: &Arc<OutgoingMessageSender>,
    thread: &AgentThread,
    adapter: Option<Arc<dyn ThreadSettingsAdapter>>,
) {
    let thread_state = outgoing.thread_state().thread_state(thread.thread_id());
    {
        let mut state = thread_state.lock().await;
        if state.listener_running {
            return;
        }
        state.listener_running = true;
    }
    let outgoing = Arc::clone(outgoing);
    let thread = thread.clone();
    tokio::spawn(async move {
        let thread_id = thread.thread_id().clone();
        while let Some(event) = thread.next_event().await {
            thread_state.lock().await.track_current_turn_event(&event);
            let connection_ids = outgoing
                .thread_state()
                .subscribed_connection_ids(&thread_id);
            let thread_outgoing = ThreadScopedOutgoingMessageSender::new(
                Arc::clone(&outgoing),
                connection_ids,
                thread_id.clone(),
            );
            if matches!(event, ThreadEvent::ThreadSettingsApplied) {
                let snapshot = settings_snapshot(&thread, adapter.as_deref());
                if let Err(error) = thread.persist_settings_snapshot(snapshot.clone()).await {
                    tracing::warn!(%error, "thread settings could not be persisted");
                }
                let changed = {
                    let mut state = thread_state.lock().await;
                    let changed = state.last_settings.as_ref() != Some(&snapshot);
                    state.last_settings = Some(snapshot.clone());
                    changed
                };
                if changed {
                    let notification = ThreadSettingsUpdatedNotification {
                        thread_id: thread_id.as_str().into(),
                        thread_settings: snapshot,
                    };
                    thread_outgoing
                        .send_server_notification(
                            ServerNotification::new(
                                THREAD_SETTINGS_UPDATED_METHOD,
                                serde_json::json!({"threadId": notification.thread_id, "threadSettings": notification.thread_settings}),
                            )
                            .experimental(THREAD_SETTINGS_UPDATED_METHOD),
                        )
                        .await;
                }
                continue;
            }
            apply_turn_event_handling(event, &thread_id, &thread_outgoing, &thread_state).await;
        }
        thread_state.lock().await.listener_running = false;
    });
}

/// Tells the thread's connections what one event means: the turn arms of Codex's
/// `apply_bespoke_event_handling`.
async fn apply_turn_event_handling(
    event: ThreadEvent,
    thread_id: &SessionId,
    outgoing: &ThreadScopedOutgoingMessageSender,
    thread_state: &Mutex<ThreadState>,
) {
    match event {
        ThreadEvent::TurnStarted {
            turn_id,
            started_at,
        } => {
            // Already done when the last turn ended; done again in case anything was left.
            outgoing.abort_pending_server_requests();
            let turn = Turn::new(turn_id, TurnStatus::InProgress).with_started_at(Some(started_at));
            send(
                outgoing,
                TURN_STARTED_METHOD,
                TurnStartedNotification::new(thread_id.as_str(), turn),
            )
            .await;
        }
        ThreadEvent::Error { turn_id, message } => {
            let error = TurnError::new(message);
            thread_state.lock().await.turn_summary.last_error = Some(error.clone());
            send(
                outgoing,
                ERROR_METHOD,
                ErrorNotification::new(error, false, thread_id.as_str(), turn_id),
            )
            .await;
        }
        ThreadEvent::TurnComplete {
            turn_id,
            completed_at,
            duration_ms,
            ..
        } => {
            outgoing.abort_pending_server_requests();
            respond_to_pending_interrupts(outgoing, thread_state).await;
            let summary = std::mem::take(&mut thread_state.lock().await.turn_summary);
            let turn = match summary.last_error {
                Some(error) => Turn::new(turn_id, TurnStatus::Failed).with_error(error),
                None => Turn::new(turn_id, TurnStatus::Completed),
            }
            .with_started_at(summary.started_at)
            .with_completion(completed_at, duration_ms);
            send(
                outgoing,
                TURN_COMPLETED_METHOD,
                TurnCompletedNotification::new(thread_id.as_str(), turn),
            )
            .await;
        }
        ThreadEvent::TurnAborted {
            turn_id,
            completed_at,
            duration_ms,
            ..
        } => {
            outgoing.abort_pending_server_requests();
            respond_to_pending_interrupts(outgoing, thread_state).await;
            let summary = std::mem::take(&mut thread_state.lock().await.turn_summary);
            let turn = Turn::new(turn_id, TurnStatus::Interrupted)
                .with_started_at(summary.started_at)
                .with_completion(completed_at, duration_ms);
            send(
                outgoing,
                TURN_COMPLETED_METHOD,
                TurnCompletedNotification::new(thread_id.as_str(), turn),
            )
            .await;
        }
        _ => {}
    }
}

/// Answers the `turn/interrupt` requests waiting for the turn that ended: Codex's
/// `respond_to_pending_interrupts`.
async fn respond_to_pending_interrupts(
    outgoing: &ThreadScopedOutgoingMessageSender,
    thread_state: &Mutex<ThreadState>,
) {
    let pending = std::mem::take(&mut thread_state.lock().await.pending_interrupts);
    for request_id in pending {
        outgoing
            .send_response(request_id, TurnInterruptResponse::new())
            .await;
    }
}

async fn send(
    outgoing: &ThreadScopedOutgoingMessageSender,
    method: &'static str,
    params: impl Serialize,
) {
    match serde_json::to_value(params) {
        Ok(params) => {
            outgoing
                .send_server_notification(ServerNotification::new(method, params))
                .await;
        }
        // The notifications are plain records, so this does not happen.
        Err(error) => tracing::error!("failed to serialize {method}: {error}"),
    }
}
