//! The thread lifecycle methods: the parts of Codex's `ThreadRequestProcessor`
//! (`request_processors/thread_processor.rs`) for `thread/unsubscribe` and
//! `thread/backgroundTerminals/*`, and its cleanup when a connection closes or the server stops.
//!
//! Unsubscribing removes only the calling connection's subscription; the thread stays loaded, its
//! turn keeps running, and its listener unloads it once it has been idle with no subscribers for
//! the server's unload delay (see [`super::thread_listener`]). A closed connection loses its
//! subscriptions the same way. The thread's server requests stay with the thread — a client that
//! subscribes again is sent those still waiting — and are withdrawn only when the thread unloads.
//!
//! # Differences from Codex
//!
//! Codex's terminal ids are integers, which it parses and pages through numerically; a terminal
//! here is named by whatever the host names it by — an execution session, whose ids are
//! time-ordered — so ids are opaque strings, sorted and compared as strings for paging, and an id
//! that names no running terminal is simply not terminated rather than refused as malformed.

use std::{collections::HashSet, sync::Arc, time::Duration};

use ra_core::session::SessionId;
use tokio::sync::Mutex;

use super::{
    outgoing::{ConnectionRequestId, OutgoingMessageSender},
    protocol::{
        ThreadBackgroundTerminal, ThreadBackgroundTerminalsCleanParams,
        ThreadBackgroundTerminalsCleanResponse, ThreadBackgroundTerminalsListParams,
        ThreadBackgroundTerminalsListResponse, ThreadBackgroundTerminalsTerminateParams,
        ThreadBackgroundTerminalsTerminateResponse, ThreadUnsubscribeParams,
        ThreadUnsubscribeResponse, ThreadUnsubscribeStatus,
    },
    thread_manager::{LoadedThread, ThreadManager},
    turn_processor::TurnMethodResult,
};
use crate::{frame::JsonRpcErrorObject, transport::ConnectionId};

/// How long the server waits for its threads to shut down when it stops: Codex's
/// `shutdown_all_threads_bounded`.
const SHUTDOWN_THREADS_TIMEOUT: Duration = Duration::from_secs(10);

/// Handles the thread lifecycle methods.
#[derive(Debug)]
pub(crate) struct ThreadRequestProcessor {
    thread_manager: Arc<ThreadManager>,
    outgoing: Arc<OutgoingMessageSender>,
    pending_thread_unloads: Arc<Mutex<HashSet<SessionId>>>,
}

impl ThreadRequestProcessor {
    pub(crate) const fn new(
        thread_manager: Arc<ThreadManager>,
        outgoing: Arc<OutgoingMessageSender>,
        pending_thread_unloads: Arc<Mutex<HashSet<SessionId>>>,
    ) -> Self {
        Self {
            thread_manager,
            outgoing,
            pending_thread_unloads,
        }
    }

    /// `thread/unsubscribe`: Codex's `thread_unsubscribe`.
    pub(crate) async fn thread_unsubscribe(
        &self,
        request_id: &ConnectionRequestId,
        params: &ThreadUnsubscribeParams,
    ) -> TurnMethodResult {
        let thread_id = SessionId::new(params.thread_id());
        if self.thread_manager.get_thread(&thread_id).is_none() {
            self.finalize_thread_teardown(&thread_id).await;
            return response(ThreadUnsubscribeResponse::new(
                ThreadUnsubscribeStatus::NotLoaded,
            ));
        }
        let was_subscribed = self
            .outgoing
            .thread_state()
            .unsubscribe_connection_from_thread(&thread_id, request_id.connection_id);
        response(ThreadUnsubscribeResponse::new(if was_subscribed {
            ThreadUnsubscribeStatus::Unsubscribed
        } else {
            ThreadUnsubscribeStatus::NotSubscribed
        }))
    }

    /// `thread/backgroundTerminals/clean`: Codex's `thread_background_terminals_clean`.
    pub(crate) async fn thread_background_terminals_clean(
        &self,
        params: &ThreadBackgroundTerminalsCleanParams,
    ) -> TurnMethodResult {
        let thread = self.load_thread(params.thread_id())?;
        if let Some(terminals) = &thread.terminals {
            terminals.clean().await;
        }
        response(ThreadBackgroundTerminalsCleanResponse::new())
    }

    /// `thread/backgroundTerminals/list`: Codex's `thread_background_terminals_list`.
    pub(crate) async fn thread_background_terminals_list(
        &self,
        params: ThreadBackgroundTerminalsListParams,
    ) -> TurnMethodResult {
        let (thread_id, cursor, limit) = params.into_parts();
        let thread = self.load_thread(&thread_id)?;
        let mut terminals: Vec<_> = match &thread.terminals {
            Some(terminals) => terminals
                .list()
                .await
                .into_iter()
                .map(ThreadBackgroundTerminal::new)
                .collect(),
            None => Vec::new(),
        };
        // Codex's process manager lists them by id, which the cursor then compares against.
        terminals.sort_by(|left, right| left.process_id().cmp(right.process_id()));
        let (data, next_cursor) = paginate_background_terminals(&terminals, cursor, limit);
        response(ThreadBackgroundTerminalsListResponse::new(data, next_cursor))
    }

    /// `thread/backgroundTerminals/terminate`: Codex's `thread_background_terminals_terminate`.
    pub(crate) async fn thread_background_terminals_terminate(
        &self,
        params: &ThreadBackgroundTerminalsTerminateParams,
    ) -> TurnMethodResult {
        let thread = self.load_thread(params.thread_id())?;
        let terminated = match &thread.terminals {
            Some(terminals) => terminals.terminate(params.process_id()).await,
            None => false,
        };
        response(ThreadBackgroundTerminalsTerminateResponse::new(terminated))
    }

    /// Forgets a closed connection's subscriptions: Codex's `connection_closed`. A thread it
    /// leaves without subscribers that is no longer loaded has what is left of it torn down.
    pub(crate) async fn connection_closed(&self, connection_id: ConnectionId) {
        let thread_ids = self
            .outgoing
            .thread_state()
            .remove_connection(connection_id);
        for thread_id in thread_ids {
            if self.thread_manager.get_thread(&thread_id).is_none() {
                self.finalize_thread_teardown(&thread_id).await;
            }
        }
    }

    /// Shuts every loaded thread down as the server stops: Codex's `shutdown_threads`.
    pub(crate) async fn shutdown_threads(&self) {
        let threads = self.thread_manager.threads();
        let shutdowns = threads.iter().map(LoadedThread::shutdown);
        if tokio::time::timeout(
            SHUTDOWN_THREADS_TIMEOUT,
            futures::future::join_all(shutdowns),
        )
        .await
        .is_err()
        {
            tracing::warn!("timed out waiting for threads to shut down; proceeding");
        }
    }

    /// Clears what the server kept of a thread that is gone: Codex's `finalize_thread_teardown`.
    async fn finalize_thread_teardown(&self, thread_id: &SessionId) {
        self.pending_thread_unloads.lock().await.remove(thread_id);
        self.outgoing.cancel_requests_for_thread(thread_id, None);
        self.outgoing
            .thread_state()
            .remove_thread_state(thread_id)
            .await;
    }

    fn load_thread(&self, thread_id: &str) -> Result<LoadedThread, JsonRpcErrorObject> {
        self.thread_manager
            .get_thread(&SessionId::new(thread_id))
            .ok_or_else(|| {
                JsonRpcErrorObject::invalid_request(format!("thread not found: {thread_id}"))
            })
    }
}

/// One page of `terminals`: Codex's `paginate_background_terminals`, on string ids.
fn paginate_background_terminals(
    terminals: &[ThreadBackgroundTerminal],
    cursor: Option<String>,
    limit: Option<u32>,
) -> (Vec<ThreadBackgroundTerminal>, Option<String>) {
    let start = cursor.map_or(0, |cursor| {
        terminals
            .iter()
            .position(|terminal| terminal.process_id() > cursor.as_str())
            .unwrap_or(terminals.len())
    });
    let effective_limit = limit
        .map_or(terminals.len(), |limit| {
            usize::try_from(limit).unwrap_or(usize::MAX)
        })
        .max(1);
    let end = start.saturating_add(effective_limit).min(terminals.len());
    let next_cursor =
        (end < terminals.len()).then(|| terminals[end - 1].process_id().to_owned());
    (terminals[start..end].to_vec(), next_cursor)
}

fn response(response: impl serde::Serialize) -> TurnMethodResult {
    serde_json::to_value(response).map(Some).map_err(|error| {
        JsonRpcErrorObject::internal_error(format!("failed to serialize response: {error}"))
    })
}
