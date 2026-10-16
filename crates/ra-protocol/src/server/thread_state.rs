//! What the server keeps about each thread: Codex's `ThreadStateManager` and `ThreadState`
//! (`app-server/src/thread_state.rs`).
//!
//! The subscription indexes say which connections a thread's messages go to. Requests are sent with
//! an event's connection snapshot; their resolution uses the current subscriptions. These indexes
//! use a synchronous mutex because test requesters register before the server runs; no lock is
//! held across an await.
//!
//! Each thread's [`ThreadState`] follows its turns as its listener reports them — the turn running
//! and its items, rebuilt as Codex rebuilds them — and holds the interrupts waiting for the running
//! turn to end. Each thread also publishes whether it has subscribers and whether a turn of it is
//! active, which its listener watches to unload it once it has had neither for a while; the server
//! counts the threads with an active turn to know when a drain may finish.
//!
//! The thread methods subscribe a connection only while it is live — initialized and not yet
//! closed — as Codex's `try_add_connection_to_thread` does, so a connection that closed while its
//! `thread/start` or `thread/resume` was in flight is not left subscribed. A thread's listener
//! also takes commands, which it carries out between the thread's events: Codex's
//! `ThreadListenerCommand`, including attaching a connection to a loaded thread for
//! `thread/resume`. Questions and permission requests use the same queue so they follow their turn's
//! start without being recorded in its rollout.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use ra_core::{session::SessionId, user_input::RequestUserInputResponse};
use ra_runtime::thread::ThreadEvent;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::{
    outgoing::ConnectionRequestId,
    protocol::{
        MessagePhase, PermissionsRequestApprovalParams, PermissionsRequestApprovalResponse, Thread,
        ThreadItem, ThreadResumeInitialTurnsPageParams, ThreadSettingsRecord,
        ToolRequestUserInputParams, Turn, TurnError, TurnStatus,
    },
    thread_history::{ItemChange, ThreadHistoryBuilder, TurnEnding},
    tool_items::ThreadToolItems,
};
use crate::transport::ConnectionId;

/// A `thread/resume` of a loaded thread, answered by the thread's listener so that the answer
/// and the connection's subscription fall between two of the thread's events: Codex's
/// `PendingThreadResumeRequest`.
#[derive(Debug)]
pub(crate) struct PendingThreadResumeRequest {
    pub(crate) request_id: ConnectionRequestId,
    /// The thread as the store and the live thread describe it, without turns.
    pub(crate) thread_summary: Thread,
    /// The turns its rollout holds, with all their items, if the answer includes turns or a page.
    pub(crate) history_turns: Vec<Turn>,
    pub(crate) include_turns: bool,
    pub(crate) initial_turns_page: Option<ThreadResumeInitialTurnsPageParams>,
    pub(crate) settings: ThreadSettingsRecord,
}

/// What a thread's listener is asked to do between the thread's events: Codex's
/// `ThreadListenerCommand`.
#[derive(Debug)]
pub(crate) enum ThreadListenerCommand {
    /// Raises a question after the listener has processed the preceding turn events. The reply
    /// goes to the waiting tool; dropping its receiver withdraws a request not yet sent.
    RequestUserInput {
        params: ToolRequestUserInputParams,
        response_tx: tokio::sync::oneshot::Sender<RequestUserInputResponse>,
    },
    /// Asks for permissions after the listener has processed the preceding turn events. The grant
    /// goes to the waiting product; dropping its receiver withdraws a request not yet sent.
    RequestPermissions {
        params: PermissionsRequestApprovalParams,
        response_tx: tokio::sync::oneshot::Sender<PermissionsRequestApprovalResponse>,
    },
    /// Answers a `thread/resume` of the loaded thread and subscribes its connection, then tells
    /// the caller it is done.
    SendThreadResumeResponse {
        request: Box<PendingThreadResumeRequest>,
        completion_tx: tokio::sync::oneshot::Sender<()>,
    },
}

/// What a thread's running turn has accumulated: Codex's `TurnSummary`, without the command
/// executions it keeps to tell an approval's early start from its tool's own.
#[cfg_attr(not(feature = "test-api"), allow(dead_code))]
#[derive(Debug, Default, Clone)]
pub(crate) struct TurnSummary {
    pub(crate) started_at: Option<i64>,
    pub(crate) last_error: Option<TurnError>,
    /// The turn's last answer so far, which `turn/completed` carries.
    pub(crate) last_agent_message: Option<ThreadItem>,
}

/// What the server keeps about one thread: Codex's `ThreadState`, for its turns.
#[cfg_attr(not(feature = "test-api"), allow(dead_code))]
#[derive(Debug, Default)]
pub(crate) struct ThreadState {
    /// The `turn/interrupt` requests answered once the running turn ends.
    pub(crate) pending_interrupts: Vec<ConnectionRequestId>,
    pub(crate) turn_summary: TurnSummary,
    /// The turn that ended last.
    pub(crate) last_terminal_turn_id: Option<String>,
    active_turn_id: Option<String>,
    /// Stops the listener reading the thread's events, if one does.
    listener_cancel: Option<CancellationToken>,
    /// Sends the listener commands, while one runs.
    listener_command_tx: Option<tokio::sync::mpsc::UnboundedSender<ThreadListenerCommand>>,
    /// Last effective settings sent, as Codex deduplicates the thread snapshot.
    pub(crate) last_settings: Option<serde_json::Value>,
    /// The turn running, rebuilt from the thread's events: Codex's `current_turn_history`.
    current_turn_history: ThreadHistoryBuilder,
}

#[cfg_attr(not(feature = "test-api"), allow(dead_code))]
impl ThreadState {
    /// The turn running, as the listener last heard.
    pub(crate) fn active_turn_id(&self) -> Option<&str> {
        self.active_turn_id.as_deref()
    }

    /// Whether a listener reads the thread's events.
    pub(crate) const fn has_listener(&self) -> bool {
        self.listener_cancel.is_some()
    }

    /// Records the listener now reading the thread's events, stopped by `cancel` and taking
    /// commands through `commands`.
    pub(crate) fn set_listener(
        &mut self,
        cancel: CancellationToken,
        commands: tokio::sync::mpsc::UnboundedSender<ThreadListenerCommand>,
    ) {
        self.listener_cancel = Some(cancel);
        self.listener_command_tx = Some(commands);
    }

    /// Where the thread's listener takes commands, while one runs: Codex's
    /// `listener_command_tx`.
    pub(crate) fn listener_command_tx(
        &self,
    ) -> Option<tokio::sync::mpsc::UnboundedSender<ThreadListenerCommand>> {
        self.listener_command_tx.clone()
    }

    /// Stops the thread's listener: Codex's `clear_listener`.
    pub(crate) fn clear_listener(&mut self) {
        if let Some(cancel) = self.listener_cancel.take() {
            cancel.cancel();
        }
        self.listener_command_tx = None;
        self.current_turn_history.reset();
    }

    /// The turn running, with all its items, as the listener has heard of it: Codex's
    /// `active_turn_snapshot`.
    pub(crate) fn active_turn_snapshot(&self) -> Option<Turn> {
        self.active_turn_id.as_ref()?;
        self.current_turn_history.active_turn_snapshot()
    }

    pub(crate) fn command_output(
        &mut self,
        event: &ra_core::event::HostEvent,
    ) -> Option<(String, String)> {
        self.current_turn_history.command_output(event)
    }

    /// Follows the thread's turns: Codex's `track_current_turn_event`. Returns what a record the
    /// event carries did to the turn's items, for the listener to report.
    pub(crate) fn track_current_turn_event(
        &mut self,
        event: &ThreadEvent,
        tool_items: Option<&dyn ThreadToolItems>,
    ) -> Vec<ItemChange> {
        let mut changes = Vec::new();
        match event {
            ThreadEvent::TurnStarted {
                turn_id,
                started_at,
            } => {
                self.turn_summary.started_at = Some(*started_at);
                self.active_turn_id = Some(turn_id.clone());
                self.current_turn_history
                    .turn_started(turn_id, Some(*started_at));
            }
            ThreadEvent::ItemStarted { turn_id, item } => {
                changes = self.current_turn_history.item_started(turn_id, item);
            }
            ThreadEvent::Item { turn_id, item } => {
                changes = self
                    .current_turn_history
                    .item(Some(turn_id), item, tool_items);
                for change in &changes {
                    if let ItemChange::Completed(item) = change
                        && is_answer(item)
                    {
                        self.turn_summary.last_agent_message = Some(item.clone());
                    }
                }
            }
            ThreadEvent::HostEvent(event) => {
                changes = self.current_turn_history.host_event(event, tool_items);
                self.current_turn_history.retain_background_turns();
            }
            ThreadEvent::Error { turn_id, message } => {
                self.current_turn_history
                    .error(turn_id, TurnError::new(message.clone()));
            }
            ThreadEvent::TurnComplete {
                turn_id,
                completed_at,
                duration_ms,
                ..
            } => {
                self.current_turn_history.turn_ended(
                    turn_id,
                    &TurnEnding {
                        status: TurnStatus::Completed,
                        error: None,
                        completed_at: Some(*completed_at),
                        duration_ms: Some(*duration_ms),
                    },
                );
                self.end_turn(turn_id);
            }
            ThreadEvent::TurnAborted {
                turn_id,
                completed_at,
                duration_ms,
                ..
            } => {
                self.current_turn_history.turn_ended(
                    turn_id,
                    &TurnEnding {
                        status: TurnStatus::Interrupted,
                        error: None,
                        completed_at: Some(*completed_at),
                        duration_ms: Some(*duration_ms),
                    },
                );
                self.end_turn(turn_id);
            }
            _ => {}
        }
        changes
    }

    fn end_turn(&mut self, turn_id: &str) {
        self.last_terminal_turn_id = Some(turn_id.to_owned());
        if self.active_turn_id.as_deref() == Some(turn_id) {
            self.active_turn_id = None;
        }
        if !self.current_turn_history.has_active_turn() {
            self.current_turn_history.retain_background_turns();
        }
    }
}

/// Whether `item` is an answer `turn/completed` carries: an agent message with text that is not
/// commentary, as Codex's `track_current_turn_event` picks it.
fn is_answer(item: &ThreadItem) -> bool {
    matches!(
        item,
        ThreadItem::AgentMessage { text, phase, .. }
            if matches!(phase, Some(MessagePhase::FinalAnswer) | None) && !text.trim().is_empty()
    )
}

/// What the manager keeps of one thread: Codex's `ThreadEntry`.
#[derive(Debug)]
struct ThreadEntry {
    state: Arc<tokio::sync::Mutex<ThreadState>>,
    connection_ids: HashSet<ConnectionId>,
    has_connections_watcher: watch::Sender<bool>,
    /// Whether a turn of the thread is active, as its listener last heard: the `Active` of
    /// Codex's thread status, for turns.
    active_watcher: watch::Sender<bool>,
}

impl Default for ThreadEntry {
    fn default() -> Self {
        Self {
            state: Arc::default(),
            connection_ids: HashSet::new(),
            has_connections_watcher: watch::channel(false).0,
            active_watcher: watch::channel(false).0,
        }
    }
}

impl ThreadEntry {
    fn update_has_connections(&self) {
        let has_connections = !self.connection_ids.is_empty();
        self.has_connections_watcher.send_if_modified(|current| {
            let changed = *current != has_connections;
            *current = has_connections;
            changed
        });
    }
}

#[derive(Debug, Default)]
struct ThreadStateManagerInner {
    /// The connections that have been initialized and not closed: Codex's `live_connections`.
    live_connections: HashSet<ConnectionId>,
    threads: HashMap<SessionId, ThreadEntry>,
    thread_ids_by_connection: HashMap<ConnectionId, HashSet<SessionId>>,
}

fn subscribe(
    state: &mut ThreadStateManagerInner,
    thread_id: &SessionId,
    connection_id: ConnectionId,
) {
    state
        .thread_ids_by_connection
        .entry(connection_id)
        .or_default()
        .insert(thread_id.clone());
    let entry = state.threads.entry(thread_id.clone()).or_default();
    entry.connection_ids.insert(connection_id);
    entry.update_has_connections();
}

/// The current subscribers of each thread, shared by all event senders, and each thread's state.
#[derive(Debug)]
pub(crate) struct ThreadStateManager {
    state: Mutex<ThreadStateManagerInner>,
    /// How many threads have an active turn, as their listeners last heard: Codex's running
    /// assistant turn count.
    running_turns: watch::Sender<usize>,
}

impl Default for ThreadStateManager {
    fn default() -> Self {
        Self {
            state: Mutex::default(),
            running_turns: watch::channel(0).0,
        }
    }
}

#[cfg_attr(not(feature = "test-api"), allow(dead_code))]
impl ThreadStateManager {
    /// The state of the thread `thread_id`, created when first asked for: Codex's
    /// `thread_state`.
    pub(crate) fn thread_state(
        &self,
        thread_id: &SessionId,
    ) -> Arc<tokio::sync::Mutex<ThreadState>> {
        Arc::clone(
            &self
                .lock()
                .threads
                .entry(thread_id.clone())
                .or_default()
                .state,
        )
    }

    /// Records that a connection has been initialized, so the thread methods may subscribe it:
    /// Codex's `connection_initialized`.
    pub(crate) fn connection_initialized(&self, connection_id: ConnectionId) {
        self.lock().live_connections.insert(connection_id);
    }

    /// Subscribes a live connection to a thread, keeping its other subscriptions: Codex's
    /// `try_add_connection_to_thread`. Returns `false`, subscribing nothing, if the connection is
    /// not live.
    pub(crate) fn try_add_connection_to_thread(
        &self,
        thread_id: &SessionId,
        connection_id: ConnectionId,
    ) -> bool {
        let mut state = self.lock();
        if !state.live_connections.contains(&connection_id) {
            return false;
        }
        subscribe(&mut state, thread_id, connection_id);
        true
    }

    /// Adds a subscription without replacing the thread's other subscriptions.
    pub(crate) fn add_connection_to_thread(
        &self,
        thread_id: &SessionId,
        connection_id: ConnectionId,
    ) {
        subscribe(&mut self.lock(), thread_id, connection_id);
    }

    pub(crate) fn subscribed_connection_ids(&self, thread_id: &SessionId) -> Vec<ConnectionId> {
        self.lock()
            .threads
            .get(thread_id)
            .map(|entry| entry.connection_ids.iter().copied().collect())
            .unwrap_or_default()
    }

    /// Removes only this connection's subscription to this thread: Codex's
    /// `unsubscribe_connection_from_thread`. Returns whether there was one.
    pub(crate) fn unsubscribe_connection_from_thread(
        &self,
        thread_id: &SessionId,
        connection_id: ConnectionId,
    ) -> bool {
        let mut state = self.lock();
        if !state.threads.contains_key(thread_id) {
            return false;
        }
        if !state
            .thread_ids_by_connection
            .get(&connection_id)
            .is_some_and(|thread_ids| thread_ids.contains(thread_id))
        {
            return false;
        }
        if let Some(thread_ids) = state.thread_ids_by_connection.get_mut(&connection_id) {
            thread_ids.remove(thread_id);
            if thread_ids.is_empty() {
                state.thread_ids_by_connection.remove(&connection_id);
            }
        }
        if let Some(entry) = state.threads.get_mut(thread_id) {
            entry.connection_ids.remove(&connection_id);
            entry.update_has_connections();
        }
        true
    }

    /// Removes a closed connection from every thread it subscribed to: Codex's
    /// `remove_connection`. Returns the threads it leaves without subscribers.
    pub(crate) fn remove_connection(&self, connection_id: ConnectionId) -> Vec<SessionId> {
        let mut state = self.lock();
        state.live_connections.remove(&connection_id);
        let thread_ids = state
            .thread_ids_by_connection
            .remove(&connection_id)
            .unwrap_or_default();
        for thread_id in &thread_ids {
            if let Some(entry) = state.threads.get_mut(thread_id) {
                entry.connection_ids.remove(&connection_id);
                entry.update_has_connections();
            }
        }
        thread_ids
            .into_iter()
            .filter(|thread_id| {
                state
                    .threads
                    .get(thread_id)
                    .is_some_and(|entry| entry.connection_ids.is_empty())
            })
            .collect()
    }

    /// Whether the thread has subscribers, as it changes: Codex's `subscribe_to_has_connections`.
    /// `None` once the thread's state has been removed.
    pub(crate) fn subscribe_to_has_connections(
        &self,
        thread_id: &SessionId,
    ) -> Option<watch::Receiver<bool>> {
        self.lock()
            .threads
            .get(thread_id)
            .map(|entry| entry.has_connections_watcher.subscribe())
    }

    /// Whether a turn of the thread is active, as it changes. `None` once the thread's state has
    /// been removed.
    pub(crate) fn subscribe_to_active(
        &self,
        thread_id: &SessionId,
    ) -> Option<watch::Receiver<bool>> {
        self.lock()
            .threads
            .get(thread_id)
            .map(|entry| entry.active_watcher.subscribe())
    }

    /// Records whether a turn of the thread is active, as its listener hears of turns starting and
    /// ending.
    pub(crate) fn note_active(&self, thread_id: &SessionId, active: bool) {
        let state = self.lock();
        if let Some(entry) = state.threads.get(thread_id) {
            entry.active_watcher.send_replace(active);
        }
        self.publish_running_turns(&state);
    }

    /// How many threads have an active turn, as it changes.
    pub(crate) fn subscribe_running_turn_count(&self) -> watch::Receiver<usize> {
        self.running_turns.subscribe()
    }

    /// Forgets the thread — its state, its subscriptions — and stops its listener: Codex's
    /// `remove_thread_state`.
    pub(crate) async fn remove_thread_state(&self, thread_id: &SessionId) {
        let thread_state = {
            let mut state = self.lock();
            let entry = state.threads.remove(thread_id);
            state.thread_ids_by_connection.retain(|_, thread_ids| {
                thread_ids.remove(thread_id);
                !thread_ids.is_empty()
            });
            self.publish_running_turns(&state);
            entry.map(|entry| entry.state)
        };
        if let Some(thread_state) = thread_state {
            thread_state.lock().await.clear_listener();
        }
    }

    fn publish_running_turns(&self, state: &ThreadStateManagerInner) {
        let running = state
            .threads
            .values()
            .filter(|entry| *entry.active_watcher.borrow())
            .count();
        self.running_turns.send_if_modified(|current| {
            let changed = *current != running;
            *current = running;
            changed
        });
    }

    fn lock(&self) -> MutexGuard<'_, ThreadStateManagerInner> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
