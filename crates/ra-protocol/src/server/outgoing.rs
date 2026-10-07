//! What the server sends and to whom: Codex's `OutgoingMessageSender` and `OutgoingEnvelope`
//! (`app-server/src/outgoing_message.rs`).
//!
//! Every message the server sends goes through one queue as an [`OutgoingEnvelope`], addressed to
//! one connection or to every connection, and the outbound router writes it out (see
//! [`super::router`]). Answers from the client to the server's own requests come back through
//! [`PendingRequests`].
//!
//! Codex's sender also keeps the tracing context of each request in flight and reports requests,
//! responses and notifications to its analytics; neither is ported.

use serde_json::Value;
use tokio::sync::mpsc;

use crate::{
    frame::{
        JsonRpcError, JsonRpcErrorObject, JsonRpcMessage, JsonRpcNotification, JsonRpcResponse,
        PendingRequests, RequestId,
    },
    transport::ConnectionId,
};

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
}

impl ServerNotification {
    /// A stable notification of `method` carrying `params`.
    #[cfg_attr(not(feature = "test-api"), allow(dead_code))]
    pub(crate) fn new(method: impl Into<String>, params: Value) -> Self {
        Self {
            method: method.into(),
            params,
            experimental_reason: None,
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
///
/// Codex's also carries the server's own requests, which arrive with them.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum OutgoingMessage {
    /// A notification, which the router may withhold from a connection.
    Notification(ServerNotification),
    /// A successful answer.
    Response(JsonRpcResponse),
    /// A failed answer.
    Error(JsonRpcError),
}

impl From<OutgoingMessage> for JsonRpcMessage {
    fn from(message: OutgoingMessage) -> Self {
        match message {
            OutgoingMessage::Notification(notification) => {
                JsonRpcNotification::new(notification.method)
                    .with_params(notification.params)
                    .into()
            }
            OutgoingMessage::Response(response) => response.into(),
            OutgoingMessage::Error(error) => error.into(),
        }
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
}

impl OutgoingMessageSender {
    pub(crate) fn new(sender: mpsc::Sender<OutgoingEnvelope>) -> Self {
        Self {
            sender,
            pending: PendingRequests::new(),
        }
    }

    /// Answers the request `request_id` with `result`.
    pub(crate) async fn send_response(&self, request_id: ConnectionRequestId, result: Value) {
        let message =
            OutgoingMessage::Response(JsonRpcResponse::new(request_id.request_id, result));
        self.send_to_connection(request_id.connection_id, message, "response")
            .await;
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
        notification: ServerNotification,
    ) {
        tracing::trace!(
            targeted_connections = connection_ids.len(),
            "app-server event: {}",
            notification.method()
        );
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
