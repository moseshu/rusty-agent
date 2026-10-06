//! The agent's requests to the host still waiting for an answer: the callback table of Codex's
//! `OutgoingMessageSender` (`app-server/src/outgoing_message.rs`).

use std::{
    collections::HashMap,
    sync::{
        Mutex, MutexGuard, PoisonError,
        atomic::{AtomicI64, Ordering},
    },
};

use ra_core::session::SessionId;
use serde_json::Value;
use tokio::sync::oneshot;

use super::{JsonRpcErrorObject, JsonRpcRequest, RequestId};

/// How the host answered a request: Codex's `ClientRequestResult`.
pub type RequestResult = Result<Value, JsonRpcErrorObject>;

/// A request waiting for its answer.
#[derive(Debug)]
struct Pending {
    callback: oneshot::Sender<RequestResult>,
    thread: Option<SessionId>,
    request: JsonRpcRequest,
}

/// The agent's requests to the host that have not been answered: Codex's request callback table.
///
/// [`register`](Self::register) gives a request the next integer id — from zero, as Codex's do —
/// and keeps it with the receiving end of its answer. The host's response or error, routed here by
/// id, completes it once; an answer to an id that is not waiting is ignored and reported as such.
/// A request can belong to a thread, so the requests of a thread can be listed — to send them
/// again to a client that reconnects — and withdrawn together.
///
/// Codex registers and sends a request in one step, through its outgoing queue. Here the table only
/// keeps the requests; whoever writes them to the host sends what [`register`](Self::register)
/// returns, and withdraws it with [`cancel_request`](Self::cancel_request) if the send fails, as
/// Codex removes the callback of a request it could not send. Codex's per-connection ownership of
/// user-verification requests belongs to its account machinery and is not ported.
#[derive(Debug, Default)]
pub struct PendingRequests {
    next_id: AtomicI64,
    requests: Mutex<HashMap<RequestId, Pending>>,
}

impl PendingRequests {
    /// A table with nothing waiting.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Gives a request for `method` with `params` the next id and keeps it, for the thread of
    /// `thread` if it belongs to one: Codex's `send_request_to_connections` without the send.
    /// Returns the request to send and where its answer will arrive.
    ///
    /// The answer channel closes without a value if the request is withdrawn without an error.
    pub fn register(
        &self,
        method: impl Into<String>,
        params: Option<Value>,
        thread: Option<SessionId>,
    ) -> (JsonRpcRequest, oneshot::Receiver<RequestResult>) {
        let id = RequestId::Integer(self.next_id.fetch_add(1, Ordering::Relaxed));
        let mut request = JsonRpcRequest::new(id.clone(), method);
        if let Some(params) = params {
            request = request.with_params(params);
        }
        let (callback, answer) = oneshot::channel();
        self.lock().insert(
            id,
            Pending {
                callback,
                thread,
                request: request.clone(),
            },
        );
        (request, answer)
    }

    /// Completes the request `id` with the host's `result`: Codex's `notify_client_response`.
    /// Returns whether a request was waiting for it.
    pub fn notify_response(&self, id: &RequestId, result: Value) -> bool {
        self.complete(id, Ok(result))
    }

    /// Completes the request `id` with the host's `error`: Codex's `notify_client_error`. Returns
    /// whether a request was waiting for it.
    pub fn notify_error(&self, id: &RequestId, error: JsonRpcErrorObject) -> bool {
        self.complete(id, Err(error))
    }

    fn complete(&self, id: &RequestId, result: RequestResult) -> bool {
        let Some(pending) = self.lock().remove(id) else {
            tracing::warn!(%id, "an answer arrived for a request that is not waiting");
            return false;
        };
        if pending.callback.send(result).is_err() {
            tracing::warn!(%id, "an answer arrived for a request no one waits on any more");
        }
        true
    }

    /// Withdraws the request `id` without answering it, closing its answer channel: Codex's
    /// `cancel_request`. Returns whether it was waiting.
    pub fn cancel_request(&self, id: &RequestId) -> bool {
        self.lock().remove(id).is_some()
    }

    /// Withdraws every request, completing each with `error` if one is given and otherwise closing
    /// its answer channel: Codex's `cancel_all_requests`.
    pub fn cancel_all(&self, error: Option<&JsonRpcErrorObject>) {
        let pending = self.lock().drain().map(|(_, pending)| pending).collect();
        settle(pending, error);
    }

    /// The requests of the thread of `thread` still waiting, in the order of their ids: Codex's
    /// `pending_requests_for_thread`, which it sends again to a client that resumes the thread.
    #[must_use]
    pub fn pending_for_thread(&self, thread: &SessionId) -> Vec<JsonRpcRequest> {
        let mut requests = self
            .lock()
            .values()
            .filter(|pending| pending.thread.as_ref() == Some(thread))
            .map(|pending| pending.request.clone())
            .collect::<Vec<_>>();
        requests.sort_by(|left, right| left.id().cmp(right.id()));
        requests
    }

    /// Withdraws the requests of the thread of `thread`, as [`Self::cancel_all`] withdraws every
    /// request: Codex's `cancel_requests_for_thread`.
    pub fn cancel_for_thread(&self, thread: &SessionId, error: Option<&JsonRpcErrorObject>) {
        let pending = {
            let mut requests = self.lock();
            let ids = requests
                .iter()
                .filter(|(_, pending)| pending.thread.as_ref() == Some(thread))
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>();
            ids.iter()
                .filter_map(|id| requests.remove(id))
                .collect::<Vec<_>>()
        };
        settle(pending, error);
    }

    /// How many requests are waiting.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether no request is waiting.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<RequestId, Pending>> {
        self.requests.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Completes withdrawn requests with `error`, or closes their answer channels without one.
fn settle(pending: Vec<Pending>, error: Option<&JsonRpcErrorObject>) {
    for pending in pending {
        if let Some(error) = error
            && pending.callback.send(Err(error.clone())).is_err()
        {
            tracing::warn!(id = %pending.request.id(), "a withdrawn request had no one waiting");
        }
    }
}
