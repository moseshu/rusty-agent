//! The task that reads a loaded thread's events and tells the thread's connections what happened:
//! Codex's thread listener (`request_processors/thread_lifecycle.rs`,
//! `ensure_listener_task_running`) and its handling of turn events
//! (`bespoke_event_handling.rs`).
//!
//! Each event is first tracked in the thread's state, then handled with the connections subscribed
//! at that moment. A record a turn adds becomes the `item/started` and `item/completed` of the
//! items it makes (see [`super::thread_history`]), and a turn that completes carries its last
//! answer in `turn/completed`, as Codex's does. A turn that starts withdraws the thread's server requests still waiting, as one
//! that ends does, since every request of a thread belongs to a turn; the end of a turn also
//! answers the `turn/interrupt` requests waiting for it. A turn whose run stops on approvals has
//! its questions asked of the client, and its answers settled, by a task of their own (see
//! [`super::approval_requests`]), so the listener goes on with the thread's events meanwhile. A
//! call of a tool the client provides is asked of the client once its item has started, and its
//! answer awaited by a task of its own too (see [`super::dynamic_tool_calls`]).
//!
//! # Unloading
//!
//! A turn's end ends neither the thread nor anything its tools left running. The listener also
//! unloads the thread, as Codex's does, once it has had no subscribers and no active turn for the
//! server's unload delay — counted from whichever of the two came last, and started afresh when a
//! subscriber returns or a turn runs. A thread whose turn still runs, waiting for approval among
//! them, is never unloaded. Unloading withdraws the thread's server requests, forgets its state,
//! shuts it down — stopping its background terminals — and, once it is down, removes it and tells
//! every connection with `thread/closed`. A thread that does not shut down in time stays loaded.
//!
//! # Statuses and commands
//!
//! As turns start and end and errors occur, the listener tells the server's
//! [`ThreadWatchManager`], which sends every connection `thread/status/changed`; an unloaded
//! thread is reported not loaded before `thread/closed`. Between events the listener carries out
//! the commands it is sent: `thread/resume` of a loaded thread is answered here, so that the turns
//! in the answer and the connection's subscription fall between the same two events, and the
//! connection misses nothing and hears nothing twice. The connection is then sent the thread's
//! server requests still waiting.
//!
//! # Differences from Codex
//!
//! Codex's listener also takes commands that order its goals and warnings with its notifications;
//! goals are not ported.

use std::{collections::HashSet, sync::Arc, time::Duration};

use ra_core::model::{ModelStreamDelta, ModelStreamDeltaKind};
use ra_core::session::SessionId;
use ra_runtime::thread::ThreadEvent;
use serde::Serialize;
use tokio::{
    sync::{Mutex, watch},
    time::Instant,
};
use tokio_util::sync::CancellationToken;

use super::{
    approval_requests::{awaiting_approval, start_approval_batch},
    dynamic_tool_calls::request_dynamic_tool_call,
    outgoing::{
        OutgoingMessageSender, ServerNotification, ThreadScopedOutgoingMessageSender,
        now_unix_timestamp_ms,
    },
    permission_requests::request_permissions,
    protocol::{
        AGENT_MESSAGE_DELTA_METHOD, AgentMessageDeltaNotification,
        COMMAND_EXECUTION_OUTPUT_DELTA_METHOD, CommandExecutionOutputDeltaNotification,
        ERROR_METHOD, ErrorNotification, ITEM_COMPLETED_METHOD, ITEM_STARTED_METHOD,
        ItemCompletedNotification, ItemStartedNotification, REASONING_SUMMARY_PART_ADDED_METHOD,
        REASONING_SUMMARY_TEXT_DELTA_METHOD, REASONING_TEXT_DELTA_METHOD,
        ReasoningSummaryPartAddedNotification, ReasoningSummaryTextDeltaNotification,
        ReasoningTextDeltaNotification, THREAD_CLOSED_METHOD, THREAD_SETTINGS_UPDATED_METHOD,
        TURN_COMPLETED_METHOD, TURN_STARTED_METHOD, ThreadClosedNotification, ThreadResumeResponse,
        ThreadSettingsUpdatedNotification, Turn, TurnCompletedNotification, TurnError,
        TurnInterruptResponse, TurnItemsView, TurnStartedNotification, TurnStatus,
    },
    settings_adapter::settings_snapshot,
    thread_history::{HistorySources, ItemChange},
    thread_manager::{LoadedThread, ThreadManager},
    thread_processor::{set_thread_status_and_interrupt_stale_turns, turns_page},
    thread_state::{PendingThreadResumeRequest, ThreadListenerCommand, ThreadState},
    thread_status::ThreadWatchManager,
    tool_items::ThreadToolItems,
    user_input_requests::request_user_input,
};
use crate::frame::JsonRpcErrorObject;

/// How long an unloading thread is given to shut down: Codex's `wait_for_thread_shutdown`.
const THREAD_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

/// What a thread's listener works with: Codex's `ListenerTaskContext`.
#[derive(Clone, Debug)]
pub(crate) struct ListenerTaskContext {
    pub(crate) outgoing: Arc<OutgoingMessageSender>,
    /// How the thread's tools' calls show as items.
    pub(crate) history: Arc<HistorySources>,
    pub(crate) thread_manager: Arc<ThreadManager>,
    /// The threads being unloaded, until they are gone or stay.
    pub(crate) pending_thread_unloads: Arc<Mutex<HashSet<SessionId>>>,
    pub(crate) thread_unload_delay: Duration,
    /// The statuses clients are told of.
    pub(crate) thread_watch: Arc<ThreadWatchManager>,
}

/// When a thread may be unloaded: Codex's `UnloadingState`, on Tokio's clock rather than the
/// system's, which is the same while the runtime's clock runs.
struct UnloadingState {
    delay: Duration,
    has_subscribers_rx: watch::Receiver<bool>,
    has_subscribers: (bool, Instant),
    active_rx: watch::Receiver<bool>,
    is_active: (bool, Instant),
}

impl UnloadingState {
    fn new(context: &ListenerTaskContext, thread_id: &SessionId) -> Option<Self> {
        let thread_state = context.outgoing.thread_state();
        let has_subscribers_rx = thread_state.subscribe_to_has_connections(thread_id)?;
        let active_rx = thread_state.subscribe_to_active(thread_id)?;
        let has_subscribers = (*has_subscribers_rx.borrow(), Instant::now());
        let is_active = (*active_rx.borrow(), Instant::now());
        Some(Self {
            delay: context.thread_unload_delay,
            has_subscribers_rx,
            has_subscribers,
            active_rx,
            is_active,
        })
    }

    fn unloading_target(&self) -> Option<Instant> {
        match (self.has_subscribers, self.is_active) {
            ((false, has_no_subscribers_since), (false, is_inactive_since)) => {
                std::cmp::max(has_no_subscribers_since, is_inactive_since).checked_add(self.delay)
            }
            _ => None,
        }
    }

    fn sync_receiver_values(&mut self) {
        let has_subscribers = *self.has_subscribers_rx.borrow();
        if self.has_subscribers.0 != has_subscribers {
            self.has_subscribers = (has_subscribers, Instant::now());
        }
        let is_active = *self.active_rx.borrow();
        if self.is_active.0 != is_active {
            self.is_active = (is_active, Instant::now());
        }
    }

    fn should_unload_now(&mut self) -> bool {
        self.sync_receiver_values();
        self.unloading_target()
            .is_some_and(|target| target <= Instant::now())
    }

    /// Restarts the countdown from now: the thread was found running.
    fn note_thread_activity_observed(&mut self) {
        if !self.is_active.0 {
            self.is_active = (false, Instant::now());
        }
    }

    /// Waits until the thread may be unloaded. Returns `false` if the thread's state has gone.
    async fn wait_for_unloading_trigger(&mut self) -> bool {
        loop {
            self.sync_receiver_values();
            let unloading_target = self.unloading_target();
            if let Some(target) = unloading_target
                && target <= Instant::now()
            {
                return true;
            }
            let unloading_sleep = async {
                if let Some(target) = unloading_target {
                    tokio::time::sleep_until(target).await;
                } else {
                    std::future::pending::<()>().await;
                }
            };
            tokio::select! {
                () = unloading_sleep => return true,
                changed = self.has_subscribers_rx.changed() => {
                    if changed.is_err() {
                        return false;
                    }
                    self.sync_receiver_values();
                },
                changed = self.active_rx.changed() => {
                    if changed.is_err() {
                        return false;
                    }
                    self.sync_receiver_values();
                },
            }
        }
    }
}

/// The error a thread that is being unloaded answers attaching with.
fn thread_closing_error(thread_id: &SessionId) -> JsonRpcErrorObject {
    JsonRpcErrorObject::invalid_request(format!(
        "thread {thread_id} is closing; retry after the thread is closed"
    ))
}

/// Starts the thread's listener unless one is already running: Codex's
/// `ensure_listener_task_running`.
///
/// # Errors
///
/// Fails if the thread's state has already been removed, as it is while the thread is unloaded.
pub(crate) async fn ensure_listener_task_running(
    context: &ListenerTaskContext,
    loaded: &LoadedThread,
) -> Result<(), JsonRpcErrorObject> {
    let thread_id = loaded.thread_id().clone();
    let Some(mut unloading_state) = UnloadingState::new(context, &thread_id) else {
        return Err(thread_closing_error(&thread_id));
    };
    let thread_state = context.outgoing.thread_state().thread_state(&thread_id);
    let cancel = CancellationToken::new();
    let (command_tx, mut command_rx) = tokio::sync::mpsc::unbounded_channel();
    {
        let mut state = thread_state.lock().await;
        if state.has_listener() {
            return Ok(());
        }
        state.set_listener(cancel.clone(), command_tx);
    }
    let context = context.clone();
    let loaded = loaded.clone();
    tokio::spawn(async move {
        let outgoing = &context.outgoing;
        let thread = &loaded.thread;
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    // The thread's state was removed: it is being unloaded or torn down.
                    break;
                }
                // Drain ready turn events before commands so a queued question follows its turn's
                // start and call records, or is discarded after its turn ends.
                event = thread.next_event() => {
                    let Some(event) = event else {
                        break;
                    };
                    let tool_items = context.history.tool_items();
                    handle_event(
                        event,
                        &loaded,
                        outgoing,
                        &context.thread_watch,
                        &thread_state,
                        tool_items.as_deref(),
                    )
                    .await;
                }
                Some(command) = command_rx.recv() => {
                    handle_thread_listener_command(&context, &loaded, &thread_state, command)
                        .await;
                }
                unloading_watchers_open = unloading_state.wait_for_unloading_trigger() => {
                    if !unloading_watchers_open {
                        break;
                    }
                    if !unloading_state.should_unload_now() {
                        continue;
                    }
                    if thread.is_running() {
                        unloading_state.note_thread_activity_observed();
                        continue;
                    }
                    {
                        let mut pending_thread_unloads = context.pending_thread_unloads.lock().await;
                        if pending_thread_unloads.contains(&thread_id) {
                            continue;
                        }
                        if !unloading_state.should_unload_now() {
                            continue;
                        }
                        pending_thread_unloads.insert(thread_id.clone());
                    }
                    unload_thread_without_subscribers(&context, loaded.clone()).await;
                    break;
                }
            }
        }
        if !cancel.is_cancelled() {
            thread_state.lock().await.clear_listener();
        }
    });
    Ok(())
}

/// Tracks one event and tells the thread's connections what it means.
async fn handle_event(
    event: ThreadEvent,
    loaded: &LoadedThread,
    outgoing: &Arc<OutgoingMessageSender>,
    thread_watch: &Arc<ThreadWatchManager>,
    thread_state: &Mutex<ThreadState>,
    tool_items: Option<&dyn ThreadToolItems>,
) {
    if let Some(adapter) = &loaded.adapter {
        adapter.on_thread_event(&loaded.run_config(), &event);
    }
    let thread_id = loaded.thread_id().clone();
    let item_changes = thread_state
        .lock()
        .await
        .track_current_turn_event(&event, tool_items);
    match &event {
        ThreadEvent::TurnStarted { .. } => {
            outgoing.thread_state().note_active(&thread_id, true);
            thread_watch.note_turn_started(&thread_id).await;
        }
        ThreadEvent::Error { .. } => thread_watch.note_system_error(&thread_id).await,
        ThreadEvent::TurnComplete { .. } | ThreadEvent::TurnAborted { .. } => {
            thread_watch.note_turn_ended(&thread_id).await;
        }
        _ => {}
    }
    let connection_ids = outgoing
        .thread_state()
        .subscribed_connection_ids(&thread_id);
    let thread_outgoing = ThreadScopedOutgoingMessageSender::new(
        Arc::clone(outgoing),
        connection_ids,
        thread_id.clone(),
    );
    if matches!(event, ThreadEvent::ThreadSettingsApplied) {
        let snapshot = settings_snapshot(&loaded.thread, loaded.adapter.as_deref());
        if let Err(error) = loaded.persist_settings_snapshot(snapshot.clone()).await {
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
        return;
    }
    if let Some(turn_id) = awaiting_approval(&event) {
        start_approval_batch(
            loaded.clone(),
            thread_outgoing,
            Arc::clone(thread_watch),
            turn_id.to_owned(),
        );
        return;
    }
    if let ThreadEvent::Item { turn_id, .. } | ThreadEvent::ItemStarted { turn_id, .. } = &event {
        send_item_changes(item_changes, &thread_id, turn_id, &thread_outgoing).await;
        return;
    }
    if let ThreadEvent::Delta { turn_id, delta } = &event {
        send_model_delta(delta, &thread_id, turn_id, &thread_outgoing).await;
        return;
    }
    if let ThreadEvent::HostEvent(event) = &event {
        handle_host_event(event, loaded, &thread_outgoing, thread_state, item_changes).await;
        return;
    }
    let ended = matches!(
        event,
        ThreadEvent::TurnComplete { .. } | ThreadEvent::TurnAborted { .. }
    );
    apply_turn_event_handling(event, &thread_id, &thread_outgoing, thread_state).await;
    // Counted as running until its end has been sent, so a drain does not disconnect the client
    // before it hears of it.
    if ended && thread_state.lock().await.active_turn_id().is_none() {
        outgoing.thread_state().note_active(&thread_id, false);
    }
}

/// Maps tool runtime events after the listener has tracked their item boundaries.
async fn handle_host_event(
    event: &ra_core::event::HostEvent,
    loaded: &LoadedThread,
    outgoing: &ThreadScopedOutgoingMessageSender,
    thread_state: &Mutex<ThreadState>,
    item_changes: Vec<ItemChange>,
) {
    let thread_id = loaded.thread_id();
    let command_output = thread_state.lock().await.command_output(event);
    send_item_changes(item_changes, thread_id, event.run_id().as_str(), outgoing).await;
    if let Some((item_id, delta)) = command_output {
        send_to_thread(
            outgoing,
            COMMAND_EXECUTION_OUTPUT_DELTA_METHOD,
            CommandExecutionOutputDeltaNotification::new(
                thread_id.as_str(),
                event.run_id().as_str(),
                item_id,
                delta,
            ),
        )
        .await;
    }
    request_dynamic_tool_call(
        event,
        &loaded.dynamic_tool_calls,
        thread_id.as_str(),
        outgoing,
    )
    .await;
}

/// Codex's item event mapping, after adapters have removed provider wire details.
async fn send_model_delta(
    delta: &ModelStreamDelta,
    thread_id: &SessionId,
    turn_id: &str,
    outgoing: &ThreadScopedOutgoingMessageSender,
) {
    let item_id = delta.item_id().as_str();
    let thread_id = thread_id.as_str();
    match delta.kind() {
        ModelStreamDeltaKind::OutputText { delta } => {
            send_to_thread(
                outgoing,
                AGENT_MESSAGE_DELTA_METHOD,
                AgentMessageDeltaNotification::new(thread_id, turn_id, item_id, delta),
            )
            .await;
        }
        ModelStreamDeltaKind::ReasoningSummaryText {
            delta,
            summary_index,
        } => {
            send_to_thread(
                outgoing,
                REASONING_SUMMARY_TEXT_DELTA_METHOD,
                ReasoningSummaryTextDeltaNotification::new(
                    thread_id,
                    turn_id,
                    item_id,
                    delta,
                    *summary_index,
                ),
            )
            .await;
        }
        ModelStreamDeltaKind::ReasoningSummaryPartAdded { summary_index } => {
            send_to_thread(
                outgoing,
                REASONING_SUMMARY_PART_ADDED_METHOD,
                ReasoningSummaryPartAddedNotification::new(
                    thread_id,
                    turn_id,
                    item_id,
                    *summary_index,
                ),
            )
            .await;
        }
        ModelStreamDeltaKind::ReasoningText {
            delta,
            content_index,
        } => {
            send_to_thread(
                outgoing,
                REASONING_TEXT_DELTA_METHOD,
                ReasoningTextDeltaNotification::new(
                    thread_id,
                    turn_id,
                    item_id,
                    delta,
                    *content_index,
                ),
            )
            .await;
        }
        _ => {}
    }
}

/// Unloads a thread that has had no subscribers and no active turn for long enough: Codex's
/// `unload_thread_without_subscribers`.
async fn unload_thread_without_subscribers(context: &ListenerTaskContext, loaded: LoadedThread) {
    let thread_id = loaded.thread_id().clone();
    tracing::info!("thread {thread_id} has no subscribers and is idle; shutting down");

    // Any pending server request of the thread can no longer be answered; withdraw them before
    // the thread goes.
    context
        .outgoing
        .cancel_requests_for_thread(&thread_id, None);
    context
        .outgoing
        .thread_state()
        .remove_thread_state(&thread_id)
        .await;

    let context = context.clone();
    tokio::spawn(async move {
        if tokio::time::timeout(THREAD_SHUTDOWN_TIMEOUT, loaded.shutdown())
            .await
            .is_err()
        {
            context
                .pending_thread_unloads
                .lock()
                .await
                .remove(&thread_id);
            tracing::warn!("thread {thread_id} shutdown timed out; leaving thread loaded");
            return;
        }
        // Another thread may have been loaded under the same id meanwhile; only the one that
        // scheduled this unload is removed.
        if !context.thread_manager.remove_thread_if_matches(&loaded) {
            tracing::info!("thread {thread_id} was replaced or removed before teardown finalized");
            context
                .pending_thread_unloads
                .lock()
                .await
                .remove(&thread_id);
            return;
        }
        context.thread_watch.remove_thread(&thread_id).await;
        broadcast(
            &context.outgoing,
            THREAD_CLOSED_METHOD,
            ThreadClosedNotification::new(thread_id.as_str()),
        )
        .await;
        context
            .pending_thread_unloads
            .lock()
            .await
            .remove(&thread_id);
    });
}

/// Carries out one command between the thread's events: Codex's
/// `handle_thread_listener_command`.
async fn handle_thread_listener_command(
    context: &ListenerTaskContext,
    loaded: &LoadedThread,
    thread_state: &Mutex<ThreadState>,
    command: ThreadListenerCommand,
) {
    match command {
        ThreadListenerCommand::RequestUserInput {
            params,
            response_tx,
        } => {
            // A cancelled call or a turn that already ended must not leave a new pending request.
            if response_tx.is_closed()
                || thread_state.lock().await.active_turn_id() != Some(params.turn_id())
            {
                return;
            }
            let thread_id = loaded.thread_id();
            let outgoing = ThreadScopedOutgoingMessageSender::new(
                Arc::clone(&context.outgoing),
                context
                    .outgoing
                    .thread_state()
                    .subscribed_connection_ids(thread_id),
                thread_id.clone(),
            );
            request_user_input(
                params,
                response_tx,
                outgoing,
                &context.thread_watch,
                thread_id,
            )
            .await;
        }
        ThreadListenerCommand::RequestPermissions {
            params,
            response_tx,
        } => {
            // A cancelled call or a turn that already ended must not leave a new pending request.
            if response_tx.is_closed()
                || thread_state.lock().await.active_turn_id() != Some(params.turn_id())
            {
                return;
            }
            let thread_id = loaded.thread_id();
            let outgoing = ThreadScopedOutgoingMessageSender::new(
                Arc::clone(&context.outgoing),
                context
                    .outgoing
                    .thread_state()
                    .subscribed_connection_ids(thread_id),
                thread_id.clone(),
            );
            request_permissions(
                params,
                response_tx,
                outgoing,
                &context.thread_watch,
                loaded.clone(),
                context.outgoing.thread_state().thread_state(thread_id),
            )
            .await;
        }
        ThreadListenerCommand::SendThreadResumeResponse {
            request,
            completion_tx,
        } => {
            handle_pending_thread_resume_request(context, loaded, thread_state, *request).await;
            let _ = completion_tx.send(());
        }
    }
}

/// Answers `thread/resume` of the loaded thread and subscribes its connection: Codex's
/// `handle_pending_thread_resume_request`.
///
/// The turns are those the rollout holds with the running one laid over them, as the listener
/// has followed it; the answer is sent before any later event of the thread, and the connection
/// is subscribed before it, so the client hears every event after the turns it was given. A
/// thread being unloaded meanwhile refuses, and a connection that closed meanwhile is not
/// subscribed and gets no answer.
async fn handle_pending_thread_resume_request(
    context: &ListenerTaskContext,
    loaded: &LoadedThread,
    thread_state: &Mutex<ThreadState>,
    pending: PendingThreadResumeRequest,
) {
    let thread_id = loaded.thread_id().clone();
    let active_turn = thread_state.lock().await.active_turn_snapshot();
    let has_live_in_progress_turn = loaded.is_running()
        || active_turn
            .as_ref()
            .is_some_and(|turn| turn.status() == TurnStatus::InProgress);
    let PendingThreadResumeRequest {
        request_id,
        thread_summary: mut thread,
        history_turns,
        include_turns,
        initial_turns_page,
        settings,
    } = pending;
    let mut turns = history_turns;
    if let Some(active_turn) = active_turn {
        turns.retain(|turn| turn.id() != active_turn.id());
        turns.push(active_turn);
    }
    let thread_status = context.thread_watch.loaded_status_for_thread(&thread_id);
    let page = match initial_turns_page {
        Some(params) => {
            let mut page_turns = turns.clone();
            super::thread_processor::normalize_thread_turns_status(
                &mut page_turns,
                thread_status.clone(),
                has_live_in_progress_turn,
            );
            match turns_page(page_turns, &params) {
                Ok(page) => Some(page),
                Err(error) => {
                    context.outgoing.send_error(request_id, error).await;
                    return;
                }
            }
        }
        None => None,
    };
    if include_turns {
        thread.turns = turns;
    }
    set_thread_status_and_interrupt_stale_turns(
        &mut thread,
        thread_status,
        has_live_in_progress_turn,
    );

    {
        let pending_thread_unloads = context.pending_thread_unloads.lock().await;
        if pending_thread_unloads.contains(&thread_id) {
            drop(pending_thread_unloads);
            context
                .outgoing
                .send_error(
                    request_id,
                    JsonRpcErrorObject::invalid_request(format!(
                        "thread {thread_id} is closing; retry thread/resume after the thread is closed"
                    )),
                )
                .await;
            return;
        }
        if !context
            .outgoing
            .thread_state()
            .try_add_connection_to_thread(&thread_id, request_id.connection_id)
        {
            tracing::debug!(%thread_id, "skipping running thread resume for closed connection");
            return;
        }
    }
    let connection_id = request_id.connection_id;
    context
        .outgoing
        .send_serialized_response(
            request_id,
            ThreadResumeResponse::new(thread, page, settings),
        )
        .await;
    context
        .outgoing
        .replay_requests_to_connection_for_thread(connection_id, &thread_id)
        .await;
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
            send_to_thread(
                outgoing,
                TURN_STARTED_METHOD,
                TurnStartedNotification::new(thread_id.as_str(), turn),
            )
            .await;
        }
        ThreadEvent::Error { turn_id, message } => {
            let error = TurnError::new(message);
            thread_state.lock().await.turn_summary.last_error = Some(error.clone());
            send_to_thread(
                outgoing,
                ERROR_METHOD,
                ErrorNotification::new(error, false, thread_id.as_str(), turn_id),
            )
            .await;
        }
        // Codex's `StreamError` arm: an intermediate state of a retry, so the turn summary keeps
        // no error and the turn does not fail on it.
        ThreadEvent::StreamError {
            turn_id,
            message,
            additional_details,
        } => {
            let error = TurnError::new(message).with_additional_details(additional_details);
            send_to_thread(
                outgoing,
                ERROR_METHOD,
                ErrorNotification::new(error, true, thread_id.as_str(), turn_id),
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
            let turn = match (summary.last_error, summary.last_agent_message) {
                (Some(error), _) => Turn::new(turn_id, TurnStatus::Failed).with_error(error),
                (None, Some(answer)) => Turn::new(turn_id, TurnStatus::Completed)
                    .with_items(vec![answer], TurnItemsView::Summary),
                (None, None) => Turn::new(turn_id, TurnStatus::Completed),
            }
            .with_started_at(summary.started_at)
            .with_completion(completed_at, duration_ms);
            send_to_thread(
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
            send_to_thread(
                outgoing,
                TURN_COMPLETED_METHOD,
                TurnCompletedNotification::new(thread_id.as_str(), turn),
            )
            .await;
        }
        _ => {}
    }
}

/// Tells the thread's connections what a record did to the turn's items: Codex's
/// `item_event_to_server_notification`, for `item/started` and `item/completed`.
async fn send_item_changes(
    changes: Vec<ItemChange>,
    thread_id: &SessionId,
    turn_id: &str,
    outgoing: &ThreadScopedOutgoingMessageSender,
) {
    for change in changes {
        let at_ms = now_unix_timestamp_ms();
        match change {
            ItemChange::Started(item) => {
                send_to_thread(
                    outgoing,
                    ITEM_STARTED_METHOD,
                    ItemStartedNotification::new(item, thread_id.as_str(), turn_id, at_ms),
                )
                .await;
            }
            ItemChange::Completed(item) => {
                send_to_thread(
                    outgoing,
                    ITEM_COMPLETED_METHOD,
                    ItemCompletedNotification::new(item, thread_id.as_str(), turn_id, at_ms),
                )
                .await;
            }
        }
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

async fn send_to_thread(
    outgoing: &ThreadScopedOutgoingMessageSender,
    method: &'static str,
    params: impl Serialize,
) {
    if let Some(notification) = notification(method, params) {
        outgoing.send_server_notification(notification).await;
    }
}

/// Sends a notification to every initialized connection.
async fn broadcast(outgoing: &OutgoingMessageSender, method: &'static str, params: impl Serialize) {
    if let Some(notification) = notification(method, params) {
        outgoing.send_server_notification(notification).await;
    }
}

fn notification(method: &'static str, params: impl Serialize) -> Option<ServerNotification> {
    match serde_json::to_value(params) {
        Ok(params) => Some(ServerNotification::new(method, params)),
        // The notifications are plain records, so this does not happen.
        Err(error) => {
            tracing::error!("failed to serialize {method}: {error}");
            None
        }
    }
}
