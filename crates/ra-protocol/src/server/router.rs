//! Writes the server's messages to the connections they are for: Codex's outbound routing
//! (`app-server/src/transport.rs`).
//!
//! What a connection receives depends on its handshake. A message for every connection reaches
//! only those that have been initialized. A notification is withheld from a connection that opted
//! out of its method, and an experimental one from a connection that did not opt into the
//! experimental API. Answers to requests are never withheld.
//!
//! A connection the server can drop — a WebSocket — is dropped when its write queue is full, so
//! one slow client does not hold up the others; a connection it cannot drop — stdio — is waited
//! on instead.
//!
//! A command approval request sent to a connection without the experimental API is first
//! stripped of its experimental `additionalPermissions`, as Codex strips it.

use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, Ordering},
    },
};

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::{
    outgoing::{OutgoingEnvelope, OutgoingMessage},
    protocol::ServerRequest,
};
use crate::{
    frame::JsonRpcMessage,
    transport::{ConnectionId, QueuedOutgoingMessage},
};

/// What the router knows of a connection: Codex's `OutboundConnectionState`.
///
/// The flags and the opted-out methods are shared with the server's own state of the connection,
/// which sets them once the connection has been initialized.
#[derive(Debug)]
pub(crate) struct OutboundConnectionState {
    pub(crate) initialized: Arc<AtomicBool>,
    pub(crate) experimental_api_enabled: Arc<AtomicBool>,
    pub(crate) opted_out_notification_methods: Arc<RwLock<HashSet<String>>>,
    pub(crate) writer: mpsc::Sender<QueuedOutgoingMessage>,
    disconnect_sender: Option<CancellationToken>,
}

impl OutboundConnectionState {
    pub(crate) const fn new(
        writer: mpsc::Sender<QueuedOutgoingMessage>,
        initialized: Arc<AtomicBool>,
        experimental_api_enabled: Arc<AtomicBool>,
        opted_out_notification_methods: Arc<RwLock<HashSet<String>>>,
        disconnect_sender: Option<CancellationToken>,
    ) -> Self {
        Self {
            initialized,
            experimental_api_enabled,
            opted_out_notification_methods,
            writer,
            disconnect_sender,
        }
    }

    const fn can_disconnect(&self) -> bool {
        self.disconnect_sender.is_some()
    }

    fn request_disconnect(&self) {
        if let Some(disconnect_sender) = &self.disconnect_sender {
            disconnect_sender.cancel();
        }
    }
}

/// Whether the connection must not receive `message`: Codex's
/// `should_skip_notification_for_connection`.
fn should_skip_notification_for_connection(
    connection_state: &OutboundConnectionState,
    message: &OutgoingMessage,
) -> bool {
    let Ok(opted_out_notification_methods) = connection_state.opted_out_notification_methods.read()
    else {
        tracing::warn!("failed to read outbound opted-out notifications");
        return false;
    };
    match message {
        OutgoingMessage::Notification(notification) => {
            if notification.experimental_reason().is_some()
                && !connection_state
                    .experimental_api_enabled
                    .load(Ordering::Acquire)
            {
                return true;
            }
            opted_out_notification_methods.contains(notification.method())
        }
        OutgoingMessage::Request(_) | OutgoingMessage::Response(_) | OutgoingMessage::Error(_) => {
            false
        }
    }
}

/// What `message` becomes for the connection: Codex's `filter_outgoing_message_for_connection`,
/// which strips the experimental fields of a command approval request for a connection without the
/// experimental API.
fn filter_outgoing_message_for_connection(
    connection_state: &OutboundConnectionState,
    message: OutgoingMessage,
) -> OutgoingMessage {
    let experimental_api_enabled = connection_state
        .experimental_api_enabled
        .load(Ordering::Acquire);
    match message {
        OutgoingMessage::Request(ServerRequest::CommandExecutionRequestApproval {
            request_id,
            mut params,
        }) => {
            if !experimental_api_enabled {
                params.strip_experimental_fields();
            }
            OutgoingMessage::Request(ServerRequest::CommandExecutionRequestApproval {
                request_id,
                params,
            })
        }
        _ => message,
    }
}

/// Forgets the connection and asks its transport to drop it. Returns whether it was known.
fn disconnect_connection(
    connections: &mut HashMap<ConnectionId, OutboundConnectionState>,
    connection_id: ConnectionId,
) -> bool {
    if let Some(connection_state) = connections.remove(&connection_id) {
        connection_state.request_disconnect();
        return true;
    }
    false
}

/// Queues `message` for the connection unless it must not receive it: Codex's
/// `send_message_to_connection`. Returns whether the connection was dropped.
async fn send_message_to_connection(
    connections: &mut HashMap<ConnectionId, OutboundConnectionState>,
    connection_id: ConnectionId,
    message: OutgoingMessage,
) -> bool {
    let Some(connection_state) = connections.get(&connection_id) else {
        tracing::warn!("dropping message for disconnected connection: {connection_id}");
        return false;
    };
    let message = filter_outgoing_message_for_connection(connection_state, message);
    if should_skip_notification_for_connection(connection_state, &message) {
        return false;
    }
    let message = match JsonRpcMessage::try_from(message) {
        Ok(message) => message,
        Err(error) => {
            tracing::error!("failed to serialize message for {connection_id}: {error}");
            return false;
        }
    };

    let writer = connection_state.writer.clone();
    let queued_message = QueuedOutgoingMessage::new(message);
    if connection_state.can_disconnect() {
        match writer.try_send(queued_message) {
            Ok(()) => false,
            Err(mpsc::error::TrySendError::Full(_)) => {
                tracing::warn!(
                    "disconnecting slow connection after outbound queue filled: {connection_id}"
                );
                disconnect_connection(connections, connection_id)
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                disconnect_connection(connections, connection_id)
            }
        }
    } else if writer.send(queued_message).await.is_err() {
        disconnect_connection(connections, connection_id)
    } else {
        false
    }
}

/// Writes `envelope` to the connection it is for, or to every initialized connection that may
/// receive it: Codex's `route_outgoing_envelope`.
pub(crate) async fn route_outgoing_envelope(
    connections: &mut HashMap<ConnectionId, OutboundConnectionState>,
    envelope: OutgoingEnvelope,
) {
    match envelope {
        OutgoingEnvelope::ToConnection {
            connection_id,
            message,
        } => {
            send_message_to_connection(connections, connection_id, message).await;
        }
        OutgoingEnvelope::Broadcast { message } => {
            let target_connections: Vec<ConnectionId> = connections
                .iter()
                .filter_map(|(connection_id, connection_state)| {
                    (connection_state.initialized.load(Ordering::Acquire)
                        && !should_skip_notification_for_connection(connection_state, &message))
                    .then_some(*connection_id)
                })
                .collect();

            for connection_id in target_connections {
                send_message_to_connection(connections, connection_id, message.clone()).await;
            }
        }
    }
}
