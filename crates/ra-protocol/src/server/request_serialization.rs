//! The order in which requests touching the same state run: Codex's `RequestSerializationQueues`
//! (`app-server/src/request_serialization.rs`).
//!
//! A method that changes a thread names the thread as its serialization scope (Codex's
//! `serialization_scope`); its requests then run one after another, in the order they arrived,
//! whichever connection sent them, while requests for other threads and unscoped ones run
//! alongside. Each request still passes its connection's gate
//! when its turn comes, so the requests of a connection that has gone are skipped, and
//! [`RequestSerializationQueues::discard_closed`] releases them at once.
//!
//! # Differences from Codex
//!
//! Codex's scopes also cover global settings, thread paths, processes, file watches, fuzzy file
//! search sessions and MCP logins, and its global reads may run together; those come with the
//! methods that use them. Only the thread scope, always exclusive, is ported.

use std::{collections::HashMap, collections::VecDeque, future::Future, pin::Pin, sync::Arc};

use tokio::sync::Mutex;

use super::connection_rpc_gate::ConnectionRpcGate;

type BoxFutureUnit = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// What a request is ordered by: Codex's `RequestSerializationQueueKey`, for threads.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) enum RequestSerializationQueueKey {
    /// The requests that change one thread.
    Thread { thread_id: String },
}

/// A request waiting for its turn, with the gate of the connection that sent it.
pub(crate) struct QueuedInitializedRequest {
    gate: Arc<ConnectionRpcGate>,
    future: BoxFutureUnit,
}

impl QueuedInitializedRequest {
    pub(crate) fn new(
        gate: Arc<ConnectionRpcGate>,
        future: impl Future<Output = ()> + Send + 'static,
    ) -> Self {
        Self {
            gate,
            future: Box::pin(future),
        }
    }

    /// Runs the request if its connection's gate is still open.
    pub(crate) async fn run(self) {
        let Self { gate, future } = self;
        gate.run(future).await;
    }
}

/// The queues, by key. A queue exists while it has requests, drained by a task of its own.
#[derive(Clone, Default)]
pub(crate) struct RequestSerializationQueues {
    inner: Arc<Mutex<HashMap<RequestSerializationQueueKey, VecDeque<QueuedInitializedRequest>>>>,
}

impl std::fmt::Debug for RequestSerializationQueues {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RequestSerializationQueues")
            .finish_non_exhaustive()
    }
}

impl RequestSerializationQueues {
    /// Releases the requests whose connection closed while they waited.
    pub(crate) async fn discard_closed(&self) {
        let mut queues = self.inner.lock().await;
        for requests in queues.values_mut() {
            requests.retain(|request| !request.gate.is_closed());
        }
    }

    /// Queues `request` behind the others of `key`, starting the queue's task if it has none.
    pub(crate) async fn enqueue(
        &self,
        key: RequestSerializationQueueKey,
        request: QueuedInitializedRequest,
    ) {
        let should_spawn = {
            let mut queues = self.inner.lock().await;
            if let Some(requests) = queues.get_mut(&key) {
                requests.push_back(request);
                false
            } else {
                queues.insert(key.clone(), VecDeque::from([request]));
                true
            }
        };

        if should_spawn {
            let queues = self.clone();
            tokio::spawn(async move { queues.drain(key).await });
        }
    }

    /// Runs the requests of `key` one at a time until none is left.
    async fn drain(self, key: RequestSerializationQueueKey) {
        loop {
            let request = {
                let mut queues = self.inner.lock().await;
                let Some(requests) = queues.get_mut(&key) else {
                    return;
                };
                let Some(request) = requests.pop_front() else {
                    queues.remove(&key);
                    return;
                };
                request
            };
            request.run().await;
        }
    }
}
