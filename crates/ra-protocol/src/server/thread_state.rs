//! What the server keeps about each thread: Codex's `ThreadStateManager` and `ThreadState`
//! (`app-server/src/thread_state.rs`).
//!
//! The subscription indexes say which connections a thread's messages go to. Requests are sent with
//! an event's connection snapshot; their resolution uses the current subscriptions. These indexes
//! use a synchronous mutex because test requesters register before the server runs; no lock is
//! held across an await.
//!
//! Each thread's [`ThreadState`] follows its turns as its listener reports them, and holds the
//! interrupts waiting for the running turn to end. Each thread also publishes whether it has
//! subscribers and whether a turn of it is active, which its listener watches to unload it once it
//! has had neither for a while; the server counts the threads with an active turn to know when a
//! drain may finish. Live-connection admission is added with the thread methods.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use ra_core::session::SessionId;
use ra_runtime::thread::ThreadEvent;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::{outgoing::ConnectionRequestId, protocol::TurnError};
use crate::transport::ConnectionId;

/// What a thread's running turn has accumulated: Codex's `TurnSummary`, without the command
/// executions and the last final answer it keeps for item notifications.
#[cfg_attr(not(feature = "test-api"), allow(dead_code))]
#[derive(Debug, Default, Clone)]
pub(crate) struct TurnSummary {
    pub(crate) started_at: Option<i64>,
    pub(crate) last_error: Option<TurnError>,
}

/// What the server keeps about one thread: Codex's `ThreadState`, for its turns.
///
/// Codex derives the running turn from the history it builds out of the thread's events; here the
/// turn events alone say it.
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
    /// Last effective settings sent, as Codex deduplicates the thread snapshot.
    pub(crate) last_settings: Option<serde_json::Value>,
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

    /// Records the listener now reading the thread's events, stopped by `cancel`.
    pub(crate) fn set_listener(&mut self, cancel: CancellationToken) {
        self.listener_cancel = Some(cancel);
    }

    /// Stops the thread's listener: Codex's `clear_listener`.
    pub(crate) fn clear_listener(&mut self) {
        if let Some(cancel) = self.listener_cancel.take() {
            cancel.cancel();
        }
    }

    /// Follows the thread's turns: Codex's `track_current_turn_event`.
    pub(crate) fn track_current_turn_event(&mut self, event: &ThreadEvent) {
        match event {
            ThreadEvent::TurnStarted {
                turn_id,
                started_at,
            } => {
                self.turn_summary.started_at = Some(*started_at);
                self.active_turn_id = Some(turn_id.clone());
            }
            ThreadEvent::TurnComplete { turn_id, .. }
            | ThreadEvent::TurnAborted { turn_id, .. } => {
                self.end_turn(turn_id);
            }
            _ => {}
        }
    }

    fn end_turn(&mut self, turn_id: &str) {
        self.last_terminal_turn_id = Some(turn_id.to_owned());
        if self.active_turn_id.as_deref() == Some(turn_id) {
            self.active_turn_id = None;
        }
    }
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
    threads: HashMap<SessionId, ThreadEntry>,
    thread_ids_by_connection: HashMap<ConnectionId, HashSet<SessionId>>,
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
        Arc::clone(&self.lock().threads.entry(thread_id.clone()).or_default().state)
    }

    /// Adds a subscription without replacing the thread's other subscriptions.
    pub(crate) fn add_connection_to_thread(
        &self,
        thread_id: &SessionId,
        connection_id: ConnectionId,
    ) {
        let mut state = self.lock();
        state
            .thread_ids_by_connection
            .entry(connection_id)
            .or_default()
            .insert(thread_id.clone());
        let entry = state.threads.entry(thread_id.clone()).or_default();
        entry.connection_ids.insert(connection_id);
        entry.update_has_connections();
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
    pub(crate) fn subscribe_to_active(&self, thread_id: &SessionId) -> Option<watch::Receiver<bool>> {
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
