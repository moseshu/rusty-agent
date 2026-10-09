//! What the server sends and to whom: Codex's `OutgoingMessageSender` and `OutgoingEnvelope`
//! (`app-server/src/outgoing_message.rs`).
//!
//! Every message the server sends goes through one queue as an [`OutgoingEnvelope`], addressed to
//! one connection or to every connection, and the outbound router writes it out (see
//! [`super::router`]). Answers from the client to the server's own requests come back through
//! [`PendingRequests`].
//!
//! The server's requests belong to a thread and go to the connections subscribed to it, through a
//! [`ThreadScopedOutgoingMessageSender`]. A request waits in [`PendingRequests`] until the client
//! answers it or the server withdraws it because its turn moved on; either way the thread's
//! connections are then sent `serverRequest/resolved`. A connection that attaches to the thread
//! later is sent the requests still waiting.
//!
//! Codex's sender also keeps the tracing context of each request in flight, reports requests,
//! responses and notifications to its analytics, and binds a user-verification request to the one
//! connection that may answer it; none of it is ported. Its requests that belong to no thread —
//! for its account tokens and attestations — are not ported either, so neither is the broadcast of
//! a request to every connection.

use std::sync::Arc;

use ra_core::session::SessionId;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use super::protocol::{
    SERVER_REQUEST_RESOLVED_METHOD, ServerRequest, ServerRequestPayload,
    ServerRequestResolvedNotification,
};
use super::thread_state::ThreadStateManager;
use crate::{
    frame::{
        JsonRpcError, JsonRpcErrorObject, JsonRpcMessage, JsonRpcNotification, JsonRpcRequest,
        JsonRpcResponse, PendingRequests, RequestId, RequestResult,
    },
    transport::ConnectionId,
};

/// The `reason` in the data of the error a request withdrawn on a turn transition completes with:
/// Codex's `TURN_TRANSITION_PENDING_REQUEST_ERROR_REASON`.
pub(crate) const TURN_TRANSITION_PENDING_REQUEST_ERROR_REASON: &str = "turnTransition";

/// What the server receives for one of its requests: the client's answer, or nothing if the
/// request was withdrawn without one.
pub(crate) type ClientAnswer = Result<RequestResult, oneshot::error::RecvError>;

/// Whether `error` is the one a request withdrawn on a turn transition completes with: Codex's
/// `is_turn_transition_server_request_error`. Such a request needs no answer passed on, because
/// the turn it was for has moved on.
pub(crate) fn is_turn_transition_server_request_error(error: &JsonRpcErrorObject) -> bool {
    error
        .data()
        .and_then(|data| data.get("reason"))
        .and_then(Value::as_str)
        == Some(TURN_TRANSITION_PENDING_REQUEST_ERROR_REASON)
}

/// The error a request withdrawn on a turn transition completes with: the one Codex's
/// `abort_pending_server_requests` builds.
#[cfg_attr(not(feature = "test-api"), allow(dead_code))]
fn turn_transition_error() -> JsonRpcErrorObject {
    JsonRpcErrorObject::internal_error("client request resolved because the turn state was changed")
        .with_data(serde_json::json!({
            "reason": TURN_TRANSITION_PENDING_REQUEST_ERROR_REASON,
        }))
}

/// A client's request, by the connection it came from: Codex's `ConnectionRequestId`. Two
/// connections may use the same id, and each is answered on its own.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ConnectionRequestId {
    pub(crate) connection_id: ConnectionId,
    pub(crate) request_id: RequestId,
}

/// A notification from the server: Codex's `ServerNotification`.
///
/// Codex's is a typed enum, from whose variants the router learns a notification's method and
/// whether it is experimental. Parameters here are JSON values, as [`crate::frame`] carries them,
/// so a notification carries its method and the reason it is experimental alongside them; the
/// notifications ported later set both where they are built.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ServerNotification {
    method: String,
    params: Value,
    experimental_reason: Option<&'static str>,
    /// When it was sent, stamped once before it is fanned out: Codex's `emitted_at_ms`.
    emitted_at_ms: Option<i64>,
}

impl ServerNotification {
    /// A stable notification of `method` carrying `params`.
    #[cfg_attr(not(feature = "test-api"), allow(dead_code))]
    pub(crate) fn new(method: impl Into<String>, params: Value) -> Self {
        Self {
            method: method.into(),
            params,
            experimental_reason: None,
            emitted_at_ms: None,
        }
    }

    /// Marks the notification experimental for `reason`, so only connections that opted into the
    /// experimental API receive it.
    #[cfg_attr(not(feature = "test-api"), allow(dead_code))]
    pub(crate) const fn experimental(mut self, reason: &'static str) -> Self {
        self.experimental_reason = Some(reason);
        self
    }

    pub(crate) fn method(&self) -> &str {
        &self.method
    }

    pub(crate) const fn experimental_reason(&self) -> Option<&'static str> {
        self.experimental_reason
    }
}

/// A message on its way to the router: Codex's `OutgoingMessage`.
// A message is moved once from its sender to the router; boxing the request would only add an
// allocation. Codex allows the same lint on its requests.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum OutgoingMessage {
    /// A request from the server, which the router may strip of experimental fields.
    #[cfg_attr(not(feature = "test-api"), allow(dead_code))]
    Request(ServerRequest),
    /// A notification, which the router may withhold from a connection.
    Notification(ServerNotification),
    /// A successful answer.
    Response(JsonRpcResponse),
    /// A failed answer.
    Error(JsonRpcError),
}

impl TryFrom<OutgoingMessage> for JsonRpcMessage {
    type Error = serde_json::Error;

    /// Writes the message as JSON-RPC. Only a request can fail, where a parameter cannot be
    /// written as JSON; Codex's transport logs and drops such a message, as the router does.
    fn try_from(message: OutgoingMessage) -> Result<Self, serde_json::Error> {
        Ok(match message {
            OutgoingMessage::Request(request) => JsonRpcRequest::try_from(request)?.into(),
            OutgoingMessage::Notification(notification) => {
                let mut frame =
                    JsonRpcNotification::new(notification.method).with_params(notification.params);
                if let Some(emitted_at_ms) = notification.emitted_at_ms {
                    frame = frame.with_emitted_at_ms(emitted_at_ms);
                }
                frame.into()
            }
            OutgoingMessage::Response(response) => response.into(),
            OutgoingMessage::Error(error) => error.into(),
        })
    }
}

/// A message and where it goes: Codex's `OutgoingEnvelope`.
///
/// Codex's envelope to one connection can also carry a signal for when the message has been
/// written; nothing sends one yet.
#[derive(Debug)]
pub(crate) enum OutgoingEnvelope {
    /// To one connection.
    ToConnection {
        connection_id: ConnectionId,
        message: OutgoingMessage,
    },
    /// To every connection that has been initialized.
    Broadcast { message: OutgoingMessage },
}

/// Sends the server's messages to the router, and routes the client's answers to the server's
/// requests: Codex's `OutgoingMessageSender`.
#[derive(Debug)]
pub(crate) struct OutgoingMessageSender {
    sender: mpsc::Sender<OutgoingEnvelope>,
    pending: PendingRequests,
    thread_state: ThreadStateManager,
}

impl OutgoingMessageSender {
    pub(crate) fn new(sender: mpsc::Sender<OutgoingEnvelope>) -> Self {
        Self {
            sender,
            pending: PendingRequests::new(),
            thread_state: ThreadStateManager::default(),
        }
    }

    pub(crate) fn thread_state(&self) -> &ThreadStateManager {
        &self.thread_state
    }

    /// Answers the request `request_id` with `result`.
    pub(crate) async fn send_response(&self, request_id: ConnectionRequestId, result: Value) {
        let message =
            OutgoingMessage::Response(JsonRpcResponse::new(request_id.request_id, result));
        self.send_to_connection(request_id.connection_id, message, "response")
            .await;
    }

    /// Answers the request `request_id` with `response`, or with an internal error if it cannot be
    /// written as JSON, as Codex does.
    pub(crate) async fn send_serialized_response(
        &self,
        request_id: ConnectionRequestId,
        response: impl serde::Serialize,
    ) {
        match serde_json::to_value(response) {
            Ok(result) => self.send_response(request_id, result).await,
            Err(error) => {
                self.send_error(
                    request_id,
                    JsonRpcErrorObject::internal_error(format!(
                        "failed to serialize response: {error}"
                    )),
                )
                .await;
            }
        }
    }

    /// Answers the request `request_id` with `error`.
    pub(crate) async fn send_error(
        &self,
        request_id: ConnectionRequestId,
        error: JsonRpcErrorObject,
    ) {
        let message = OutgoingMessage::Error(JsonRpcError::new(request_id.request_id, error));
        self.send_to_connection(request_id.connection_id, message, "error")
            .await;
    }

    /// Sends `notification` to every connection that has been initialized.
    #[cfg_attr(not(feature = "test-api"), allow(dead_code))]
    pub(crate) async fn send_server_notification(&self, notification: ServerNotification) {
        self.send_server_notification_to_connections(&[], notification)
            .await;
    }

    /// Sends `notification` to each of `connection_ids`, or to every connection that has been
    /// initialized when there are none, as Codex's `send_server_notification_to_connections`.
    #[cfg_attr(not(feature = "test-api"), allow(dead_code))]
    pub(crate) async fn send_server_notification_to_connections(
        &self,
        connection_ids: &[ConnectionId],
        mut notification: ServerNotification,
    ) {
        tracing::trace!(
            targeted_connections = connection_ids.len(),
            "app-server event: {}",
            notification.method()
        );
        notification.emitted_at_ms = Some(now_unix_timestamp_ms());
        let message = OutgoingMessage::Notification(notification);
        if connection_ids.is_empty() {
            if let Err(error) = self
                .sender
                .send(OutgoingEnvelope::Broadcast { message })
                .await
            {
                tracing::warn!("failed to send server notification to client: {error:?}");
            }
            return;
        }
        for &connection_id in connection_ids {
            if let Err(error) = self
                .sender
                .send(OutgoingEnvelope::ToConnection {
                    connection_id,
                    message: message.clone(),
                })
                .await
            {
                tracing::warn!("failed to send server notification to client: {error:?}");
            }
        }
    }

    /// Sends `payload` with the next id to each of `connection_ids`, for the thread of
    /// `thread_id`, and keeps it until the client answers: Codex's `send_request_to_connections`.
    /// Returns its id and where its answer will arrive.
    ///
    /// A request for no connection is still kept, so a connection that attaches to the thread later
    /// is sent it. If it cannot be sent, it is withdrawn and its answer channel closes, as in
    /// Codex. If it cannot be written as JSON, it completes at once with an internal error, which
    /// its waiter reads as a failed request.
    #[cfg_attr(not(feature = "test-api"), allow(dead_code))]
    pub(crate) async fn send_request_to_connections(
        &self,
        connection_ids: &[ConnectionId],
        payload: ServerRequestPayload,
        thread_id: Option<SessionId>,
    ) -> (RequestId, oneshot::Receiver<RequestResult>) {
        // `PendingRequests` gives the id and keeps the request as JSON-RPC, so the request is
        // written first; Codex keeps the typed request and writes it in its transport.
        let placeholder = payload.clone().request_with_id(RequestId::Integer(0));
        let method = placeholder.method_name();
        let params = match JsonRpcRequest::try_from(placeholder) {
            Ok(request) => request.into_params(),
            Err(error) => {
                let (request, answer) = self.pending.register(method, None, thread_id);
                tracing::error!("failed to serialize request {}: {error}", request.id());
                self.pending.notify_error(
                    request.id(),
                    JsonRpcErrorObject::internal_error(format!(
                        "failed to serialize request: {error}"
                    )),
                );
                return (request.id().clone(), answer);
            }
        };
        let (registered, answer) = self.pending.register(method, params, thread_id);
        let id = registered.id().clone();
        let request = payload.request_with_id(id.clone());

        for &connection_id in connection_ids {
            if let Err(error) = self
                .sender
                .send(OutgoingEnvelope::ToConnection {
                    connection_id,
                    message: OutgoingMessage::Request(request.clone()),
                })
                .await
            {
                tracing::warn!("failed to send request {id} to client: {error:?}");
                self.pending.cancel_request(&id);
                break;
            }
        }
        (id, answer)
    }

    /// Sends `connection_id` the requests of the thread of `thread_id` still waiting, in the order
    /// of their ids: Codex's `replay_requests_to_connection_for_thread`, for a connection that
    /// attaches to the thread.
    #[cfg_attr(not(feature = "test-api"), allow(dead_code))]
    pub(crate) async fn replay_requests_to_connection_for_thread(
        &self,
        connection_id: ConnectionId,
        thread_id: &SessionId,
    ) {
        for request in self.pending_requests_for_thread(thread_id) {
            if let Err(error) = self
                .sender
                .send(OutgoingEnvelope::ToConnection {
                    connection_id,
                    message: OutgoingMessage::Request(request),
                })
                .await
            {
                tracing::warn!("failed to resend request to client: {error:?}");
            }
        }
    }

    /// The requests of the thread of `thread_id` still waiting, in the order of their ids: Codex's
    /// `pending_requests_for_thread`.
    #[cfg_attr(not(feature = "test-api"), allow(dead_code))]
    pub(crate) fn pending_requests_for_thread(&self, thread_id: &SessionId) -> Vec<ServerRequest> {
        self.pending
            .pending_for_thread(thread_id)
            .into_iter()
            .filter_map(|request| {
                let id = request.id().clone();
                ServerRequest::try_from(request)
                    .inspect_err(|error| {
                        tracing::warn!("failed to read pending request {id}: {error}");
                    })
                    .ok()
            })
            .collect()
    }

    /// Withdraws the requests of the thread of `thread_id`, completing each with `error` if one is
    /// given and otherwise closing its answer channel: Codex's `cancel_requests_for_thread`.
    #[cfg_attr(not(feature = "test-api"), allow(dead_code))]
    pub(crate) fn cancel_requests_for_thread(
        &self,
        thread_id: &SessionId,
        error: Option<&JsonRpcErrorObject>,
    ) {
        self.pending.cancel_for_thread(thread_id, error);
    }

    /// Completes the server's request `id` with the client's `result`: Codex's
    /// `notify_client_response`.
    pub(crate) fn notify_client_response(&self, id: &RequestId, result: Value) {
        self.pending.notify_response(id, result);
    }

    /// Completes the server's request `id` with the client's `error`: Codex's
    /// `notify_client_error`.
    pub(crate) fn notify_client_error(&self, id: &RequestId, error: JsonRpcErrorObject) {
        self.pending.notify_error(id, error);
    }

    async fn send_to_connection(
        &self,
        connection_id: ConnectionId,
        message: OutgoingMessage,
        message_kind: &'static str,
    ) {
        if let Err(error) = self
            .sender
            .send(OutgoingEnvelope::ToConnection {
                connection_id,
                message,
            })
            .await
        {
            tracing::warn!("failed to send {message_kind} to client: {error:?}");
        }
    }
}

/// Sends what concerns one thread to the connections subscribed to it: Codex's
/// `ThreadScopedOutgoingMessageSender`.
///
/// The connections are those subscribed when it was made, as in Codex, where each event of a
/// thread is handled with the connections subscribed at the time.
#[cfg_attr(not(feature = "test-api"), allow(dead_code))]
#[derive(Debug, Clone)]
pub(crate) struct ThreadScopedOutgoingMessageSender {
    outgoing: Arc<OutgoingMessageSender>,
    connection_ids: Arc<Vec<ConnectionId>>,
    thread_id: SessionId,
}

#[cfg_attr(not(feature = "test-api"), allow(dead_code))]
impl ThreadScopedOutgoingMessageSender {
    pub(crate) fn new(
        outgoing: Arc<OutgoingMessageSender>,
        connection_ids: Vec<ConnectionId>,
        thread_id: SessionId,
    ) -> Self {
        Self {
            outgoing,
            connection_ids: Arc::new(connection_ids),
            thread_id,
        }
    }

    /// Sends `payload` to the thread's connections and keeps it for the thread: Codex's
    /// `send_request`.
    pub(crate) async fn send_request(
        &self,
        payload: ServerRequestPayload,
    ) -> (RequestId, oneshot::Receiver<RequestResult>) {
        self.outgoing
            .send_request_to_connections(
                &self.connection_ids,
                payload,
                Some(self.thread_id.clone()),
            )
            .await
    }

    /// Sends `notification` to the thread's connections, and to none if it has none: Codex's
    /// `send_server_notification`, which, unlike the unscoped sender, does not fall back to every
    /// connection.
    pub(crate) async fn send_server_notification(&self, notification: ServerNotification) {
        if self.connection_ids.is_empty() {
            return;
        }
        self.outgoing
            .send_server_notification_to_connections(&self.connection_ids, notification)
            .await;
    }

    /// Answers the client's request `request_id` with `response`: Codex's `send_response`.
    #[cfg_attr(not(feature = "test-api"), allow(dead_code))]
    pub(crate) async fn send_response(
        &self,
        request_id: ConnectionRequestId,
        response: impl serde::Serialize,
    ) {
        self.outgoing
            .send_serialized_response(request_id, response)
            .await;
    }

    /// Withdraws the thread's requests still waiting, completing each with the turn transition
    /// error: Codex's `abort_pending_server_requests`, which it calls when a turn starts, completes
    /// or is aborted, since every request of a thread belongs to a turn.
    pub(crate) fn abort_pending_server_requests(&self) {
        self.outgoing
            .cancel_requests_for_thread(&self.thread_id, Some(&turn_transition_error()));
    }

    /// Tells the thread's connections that the request `request_id` needs no answer any more:
    /// Codex's `resolve_pending_server_request`.
    ///
    /// Codex sends it through the thread listener using the current subscriptions. The listener
    /// ordering is added with thread methods; the shared subscription indexes already ensure
    /// connections attached after this sender was made receive the resolution too.
    pub(crate) async fn resolve_server_request(&self, request_id: RequestId) {
        let notification =
            ServerRequestResolvedNotification::new(self.thread_id.as_str(), request_id);
        match serde_json::to_value(notification) {
            Ok(params) => {
                let connections = self
                    .outgoing
                    .thread_state()
                    .subscribed_connection_ids(&self.thread_id);
                let current = Self::new(
                    Arc::clone(&self.outgoing),
                    connections,
                    self.thread_id.clone(),
                );
                current
                    .send_server_notification(ServerNotification::new(
                        SERVER_REQUEST_RESOLVED_METHOD,
                        params,
                    ))
                    .await;
            }
            Err(error) => {
                tracing::error!("failed to serialize serverRequest/resolved: {error}");
            }
        }
    }

    /// Waits for the answer to the request `request_id`, then tells the thread's connections it is
    /// resolved: the first two steps of each of Codex's handlers of an approval or a question for
    /// the user, which resolve the request whether it was answered or withdrawn.
    ///
    /// Codex's handler of a dynamic tool call waits without resolving; so does its caller here.
    pub(crate) async fn wait_for_answer(
        &self,
        request_id: RequestId,
        answer: oneshot::Receiver<RequestResult>,
    ) -> ClientAnswer {
        let answer = answer.await;
        self.resolve_server_request(request_id).await;
        answer
    }
}

/// Milliseconds since the Unix epoch: Codex's `now_unix_timestamp_ms`.
pub(crate) fn now_unix_timestamp_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_millis()).ok())
        .unwrap_or_default()
}
