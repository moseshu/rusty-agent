//! What the server keeps about each thread: Codex's `ThreadStateManager` and `ThreadState`
//! (`app-server/src/thread_state.rs`).
//!
//! The subscription indexes say which connections a thread's messages go to. Requests are sent with
//! an event's connection snapshot; their resolution uses the current subscriptions. These indexes
//! use a synchronous mutex because test requesters register before the server runs; no lock is
//! held across an await.
//!
//! Each thread's [`ThreadState`] follows its turns as its listener reports them, and holds the
//! interrupts waiting for the running turn to end. Live-connection admission is added with the
//! thread methods.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use ra_core::session::SessionId;
use ra_runtime::thread::ThreadEvent;

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
    /// Whether a listener reads the thread's events.
    pub(crate) listener_running: bool,
    /// Last effective settings sent, as Codex deduplicates the thread snapshot.
    pub(crate) last_settings: Option<serde_json::Value>,
}

#[cfg_attr(not(feature = "test-api"), allow(dead_code))]
impl ThreadState {
    /// The turn running, as the listener last heard.
    pub(crate) fn active_turn_id(&self) -> Option<&str> {
        self.active_turn_id.as_deref()
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

#[derive(Debug, Default)]
struct ThreadStateManagerInner {
    connection_ids_by_thread: HashMap<SessionId, HashSet<ConnectionId>>,
    thread_ids_by_connection: HashMap<ConnectionId, HashSet<SessionId>>,
    threads: HashMap<SessionId, Arc<tokio::sync::Mutex<ThreadState>>>,
}

/// The current subscribers of each thread, shared by all event senders, and each thread's state.
#[derive(Debug, Default)]
pub(crate) struct ThreadStateManager {
    state: Mutex<ThreadStateManagerInner>,
}

#[cfg_attr(not(feature = "test-api"), allow(dead_code))]
impl ThreadStateManager {
    /// The state of the thread `thread_id`, created when first asked for: Codex's
    /// `thread_state`.
    pub(crate) fn thread_state(
        &self,
        thread_id: &SessionId,
    ) -> Arc<tokio::sync::Mutex<ThreadState>> {
        Arc::clone(self.lock().threads.entry(thread_id.clone()).or_default())
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
        state
            .connection_ids_by_thread
            .entry(thread_id.clone())
            .or_default()
            .insert(connection_id);
    }

    pub(crate) fn subscribed_connection_ids(&self, thread_id: &SessionId) -> Vec<ConnectionId> {
        self.lock()
            .connection_ids_by_thread
            .get(thread_id)
            .map(|ids| ids.iter().copied().collect())
            .unwrap_or_default()
    }

    /// Removes only this connection's subscription to this thread.
    pub(crate) fn unsubscribe_connection_from_thread(
        &self,
        thread_id: &SessionId,
        connection_id: ConnectionId,
    ) -> bool {
        let mut state = self.lock();
        let Some(ids) = state.connection_ids_by_thread.get_mut(thread_id) else {
            return false;
        };
        if !ids.remove(&connection_id) {
            return false;
        }
        if let Some(threads) = state.thread_ids_by_connection.get_mut(&connection_id) {
            threads.remove(thread_id);
            if threads.is_empty() {
                state.thread_ids_by_connection.remove(&connection_id);
            }
        }
        true
    }

    /// Removes a closed connection from every thread it subscribed to.
    pub(crate) fn remove_connection(&self, connection_id: ConnectionId) {
        let mut state = self.lock();
        let threads = state
            .thread_ids_by_connection
            .remove(&connection_id)
            .unwrap_or_default();
        for thread_id in threads {
            if let Some(ids) = state.connection_ids_by_thread.get_mut(&thread_id) {
                ids.remove(&connection_id);
            }
        }
    }

    fn lock(&self) -> MutexGuard<'_, ThreadStateManagerInner> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
