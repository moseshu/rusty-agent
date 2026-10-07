//! The subscription indexes of Codex's `ThreadStateManager` (`app-server/src/thread_state.rs`).
//! Requests are sent with an event's connection snapshot; their resolution uses the current
//! subscriptions. Thread listeners and live-connection admission are added with thread methods.
//! These indexes use a synchronous mutex because test requesters register before the server runs;
//! no lock is held across an await.

use std::{
    collections::{HashMap, HashSet},
    sync::{Mutex, MutexGuard, PoisonError},
};

use ra_core::session::SessionId;

use crate::transport::ConnectionId;

#[derive(Debug, Default)]
struct ThreadStateManagerInner {
    connection_ids_by_thread: HashMap<SessionId, HashSet<ConnectionId>>,
    thread_ids_by_connection: HashMap<ConnectionId, HashSet<SessionId>>,
}

/// The current subscribers of each thread, shared by all event senders.
#[derive(Debug, Default)]
pub(crate) struct ThreadStateManager {
    state: Mutex<ThreadStateManagerInner>,
}

#[cfg_attr(not(feature = "test-api"), allow(dead_code))]
impl ThreadStateManager {
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
