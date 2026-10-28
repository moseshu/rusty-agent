// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! The order in which requests touching the same state run: Codex's `RequestSerializationQueues`
//! (`app-server/src/request_serialization.rs`).
//!
//! A request names its serialization scope; requests with the same key run in the order they
//! arrived, whichever connection sent them, while requests for other keys and unscoped ones run
//! alongside. An exclusive request runs alone. Consecutive shared reads of one key run together,
//! and a shared read that arrives while such a batch runs joins it unless an exclusive request is
//! already waiting, so a write is never overtaken by later reads. MCP registry methods share one
//! exclusive global queue; product configuration, plugin and skill writes share the exclusive
//! `config` queue, and their reads take it shared. Each request still passes its connection's
//! gate when its turn comes, so the requests of a connection that has gone are skipped, and
//! [`RequestSerializationQueues::discard_closed`] releases them at once.
//!
//! # Differences from Codex
//!
//! Codex's scopes also cover thread paths, processes, file watches, fuzzy file search sessions
//! and MCP logins; those come with the methods that use them. Its app-owned background entries
//! only serve account-backed remote plugin hook trust, which is not part of this product.
use std::collections::HashMap;
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use futures::stream::FuturesUnordered;
use futures::stream::StreamExt;
use tokio::sync::Mutex;
use tokio::sync::Notify;
use tracing::Instrument;

use super::connection_rpc_gate::ConnectionRpcGate;
use super::protocol::ClientRequestSerializationScope;

type BoxFutureUnit = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// What a request is ordered by: Codex's `RequestSerializationQueueKey`.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) enum RequestSerializationQueueKey {
    /// A global resource, shared across connections.
    Global(&'static str),
    /// The requests that change one thread.
    Thread { thread_id: String },
}

/// How a request holds its key: alone, or together with neighbouring shared reads.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RequestSerializationAccess {
    Exclusive,
    SharedRead,
}

impl RequestSerializationQueueKey {
    /// The key and access a request's declared scope maps to.
    pub(crate) fn from_scope(
        scope: ClientRequestSerializationScope,
    ) -> (Self, RequestSerializationAccess) {
        match scope {
            ClientRequestSerializationScope::Global(name) => {
                (Self::Global(name), RequestSerializationAccess::Exclusive)
            }
            ClientRequestSerializationScope::GlobalSharedRead(name) => {
                (Self::Global(name), RequestSerializationAccess::SharedRead)
            }
            ClientRequestSerializationScope::Thread { thread_id } => (
                Self::Thread { thread_id },
                RequestSerializationAccess::Exclusive,
            ),
        }
    }
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

struct QueuedSerializedRequest {
    access: RequestSerializationAccess,
    request: QueuedInitializedRequest,
}

struct RequestSerializationQueue {
    requests: VecDeque<QueuedSerializedRequest>,
    changed: Arc<Notify>,
}

/// The queues, by key. A queue exists while it has requests, drained by a task of its own.
#[derive(Clone, Default)]
pub(crate) struct RequestSerializationQueues {
    inner: Arc<Mutex<HashMap<RequestSerializationQueueKey, RequestSerializationQueue>>>,
}

impl RequestSerializationQueues {
    /// Release requests whose connection closed while they waited in a queue.
    pub(crate) async fn discard_closed(&self) {
        let mut queues = self.inner.lock().await;
        for queue in queues.values_mut() {
            let previous_len = queue.requests.len();
            queue
                .requests
                .retain(|request| !request.request.gate.is_closed());
            if queue.requests.len() != previous_len {
                queue.changed.notify_one();
            }
        }
    }

    /// Queues a request behind the earlier requests of its key.
    pub(crate) async fn enqueue(
        &self,
        key: RequestSerializationQueueKey,
        access: RequestSerializationAccess,
        request: QueuedInitializedRequest,
    ) {
        let request = QueuedSerializedRequest { access, request };
        let should_spawn = {
            let mut queues = self.inner.lock().await;
            if let Some(queue) = queues.get_mut(&key) {
                queue.requests.push_back(request);
                queue.changed.notify_one();
                false
            } else {
                let mut requests = VecDeque::new();
                requests.push_back(request);
                let queue = RequestSerializationQueue {
                    requests,
                    changed: Arc::new(Notify::new()),
                };
                queues.insert(key.clone(), queue);
                true
            }
        };

        if should_spawn {
            let queues = self.clone();
            let span = tracing::debug_span!("app_server.serialized_request_queue", ?key);
            tokio::spawn(async move { queues.drain(key).await }.instrument(span));
        }
    }

    async fn drain(self, key: RequestSerializationQueueKey) {
        loop {
            let (requests, changed) = {
                let mut queues = self.inner.lock().await;
                let Some(queue) = queues.get_mut(&key) else {
                    return;
                };
                let Some(request) = queue.requests.pop_front() else {
                    queues.remove(&key);
                    return;
                };
                let access = request.access;
                let mut requests = vec![request];
                if access == RequestSerializationAccess::SharedRead {
                    while queue.requests.front().is_some_and(|request| {
                        request.access == RequestSerializationAccess::SharedRead
                    }) {
                        let Some(request) = queue.requests.pop_front() else {
                            break;
                        };
                        requests.push(request);
                    }
                }
                (requests, Arc::clone(&queue.changed))
            };

            if requests[0].access == RequestSerializationAccess::Exclusive {
                for request in requests {
                    request.request.run().await;
                }
                continue;
            }

            let mut running_reads = requests
                .into_iter()
                .map(|request| request.request.run())
                .collect::<FuturesUnordered<_>>();

            loop {
                tokio::select! {
                    Some(()) = running_reads.next() => {
                        if running_reads.is_empty() {
                            break;
                        }
                    }
                    () = changed.notified() => {
                        let requests = {
                            let mut queues = self.inner.lock().await;
                            let Some(queue) = queues.get_mut(&key) else {
                                return;
                            };
                            let mut requests = Vec::new();
                            while queue.requests.front().is_some_and(|request| {
                                request.access == RequestSerializationAccess::SharedRead
                            }) {
                                let Some(request) = queue.requests.pop_front() else {
                                    break;
                                };
                                requests.push(request);
                            }
                            requests
                        };

                        for request in requests {
                            running_reads.push(request.request.run());
                        }
                    }
                }
            }
        }
    }
}

impl std::fmt::Debug for RequestSerializationQueues {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RequestSerializationQueues")
            .finish_non_exhaustive()
    }
}
