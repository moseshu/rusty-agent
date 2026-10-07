//! How messages travel: one JSON message per line over stdio, or WebSocket frames, as Codex's
//! app-server transports carry them (`app-server-transport/src/transport/mod.rs`,
//! `app-server-transport/src/outgoing_message.rs`).
//!
//! A transport turns each connection into [`TransportEvent`]s on one channel the server reads: the
//! connection opening, with the queue that writes to it, each message it sends, and its closing.
//! Every transport shares the same handling of what arrives. A line that is not a message is logged
//! and skipped, and the connection stays open. When the server's channel is full, a request is
//! answered at once with [`error_code::OVERLOADED`](crate::frame::error_code::OVERLOADED) so the
//! client may retry, while responses and notifications wait for room, since dropping an answer
//! would leave the request it answers waiting forever.
//!
//! Codex's outgoing messages are typed requests, notifications and results, and a result that
//! fails to serialize goes out as an internal error instead. Here parameters and results are JSON
//! values, as [`crate::frame`] carries them, so an outgoing message is a [`JsonRpcMessage`] and
//! writing it cannot fail that way.

pub mod stdio;
pub mod websocket;

use std::{
    fmt,
    sync::atomic::{AtomicU64, Ordering},
};

use tokio::sync::{mpsc, oneshot};

use crate::frame::{JsonRpcError, JsonRpcErrorObject, JsonRpcMessage};

/// The capacity of the bounded channels between a transport and the server: Codex's
/// `CHANNEL_CAPACITY`, a balance between throughput and memory that is plenty for an interactive
/// client.
pub const CHANNEL_CAPACITY: usize = 128;

/// Which connection a message came from or goes to: Codex's `ConnectionId`.
///
/// Transports number their connections from zero, in the order they open, across the process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConnectionId(u64);

impl ConnectionId {
    /// The connection numbered `value`.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Its number.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for ConnectionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// The transport a connection arrived on: Codex's `ConnectionOrigin`.
///
/// Codex also has in-process and remote-control connections; remote control is not ported, and the
/// other transports add their origin when they land.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionOrigin {
    /// The process's stdin and stdout.
    Stdio,
}

/// What a transport tells the server: Codex's `TransportEvent`.
#[non_exhaustive]
#[derive(Debug)]
pub enum TransportEvent {
    /// A connection opened; what the server sends it goes through `writer`.
    ///
    /// Codex's event also carries the connection's authentication and a token to disconnect it,
    /// which only its WebSocket transport fills in; they arrive with that transport.
    #[non_exhaustive]
    ConnectionOpened {
        /// The connection.
        connection_id: ConnectionId,
        /// The transport it arrived on.
        origin: ConnectionOrigin,
        /// The queue of messages to write to it.
        writer: mpsc::Sender<QueuedOutgoingMessage>,
    },
    /// A connection closed; nothing more will arrive from it.
    ConnectionClosed {
        /// The connection.
        connection_id: ConnectionId,
    },
    /// A connection sent a message.
    IncomingMessage {
        /// The connection.
        connection_id: ConnectionId,
        /// The message.
        message: JsonRpcMessage,
    },
}

/// A message waiting to be written to a connection: Codex's `QueuedOutgoingMessage`.
#[derive(Debug)]
pub struct QueuedOutgoingMessage {
    message: JsonRpcMessage,
    write_complete: Option<oneshot::Sender<()>>,
}

impl QueuedOutgoingMessage {
    /// A message to write.
    #[must_use]
    pub fn new(message: impl Into<JsonRpcMessage>) -> Self {
        Self {
            message: message.into(),
            write_complete: None,
        }
    }

    /// Signals `write_complete` once the message has been written, as Codex's `write_complete_tx`
    /// does. If the message is never written, the signal's channel closes instead.
    #[must_use]
    pub fn with_write_complete(mut self, write_complete: oneshot::Sender<()>) -> Self {
        self.write_complete = Some(write_complete);
        self
    }

    /// The message.
    #[must_use]
    pub const fn message(&self) -> &JsonRpcMessage {
        &self.message
    }

    /// Takes the message.
    #[must_use]
    pub fn into_message(self) -> JsonRpcMessage {
        self.message
    }

    fn into_parts(self) -> (JsonRpcMessage, Option<oneshot::Sender<()>>) {
        (self.message, self.write_complete)
    }
}

static CONNECTION_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

fn next_connection_id() -> ConnectionId {
    ConnectionId(CONNECTION_ID_COUNTER.fetch_add(1, Ordering::Relaxed))
}

/// Parses `payload` and hands it to the server: Codex's `forward_incoming_message`. Returns whether
/// the connection should keep reading; a payload that is not a message is logged and skipped.
async fn forward_incoming_message(
    transport_event_tx: &mpsc::Sender<TransportEvent>,
    writer: &mpsc::Sender<QueuedOutgoingMessage>,
    connection_id: ConnectionId,
    payload: &str,
) -> bool {
    match serde_json::from_str::<JsonRpcMessage>(payload) {
        Ok(message) => {
            enqueue_incoming_message(transport_event_tx, writer, connection_id, message).await
        }
        Err(error) => {
            tracing::error!("Failed to deserialize JSONRPCMessage: {error}");
            true
        }
    }
}

/// Hands `message` to the server: Codex's `enqueue_incoming_message`. Returns whether the
/// connection should keep reading, which it should not once the server has gone.
///
/// When the server's channel is full, a request is answered on `writer` with the overloaded error
/// instead of waiting — and if `writer` is full too, the answer is dropped rather than block
/// reading — while any other message waits for room.
async fn enqueue_incoming_message(
    transport_event_tx: &mpsc::Sender<TransportEvent>,
    writer: &mpsc::Sender<QueuedOutgoingMessage>,
    connection_id: ConnectionId,
    message: JsonRpcMessage,
) -> bool {
    let event = TransportEvent::IncomingMessage {
        connection_id,
        message,
    };
    match transport_event_tx.try_send(event) {
        Ok(()) => true,
        Err(mpsc::error::TrySendError::Closed(_)) => false,
        Err(mpsc::error::TrySendError::Full(TransportEvent::IncomingMessage {
            connection_id,
            message: JsonRpcMessage::Request(request),
        })) => {
            let overload_error =
                JsonRpcError::new(request.id().clone(), JsonRpcErrorObject::overloaded());
            match writer.try_send(QueuedOutgoingMessage::new(overload_error)) {
                Ok(()) => true,
                Err(mpsc::error::TrySendError::Closed(_)) => false,
                Err(mpsc::error::TrySendError::Full(_overload_error)) => {
                    tracing::warn!(
                        "dropping overload response for connection {connection_id}: outbound queue is full"
                    );
                    true
                }
            }
        }
        Err(mpsc::error::TrySendError::Full(event)) => transport_event_tx.send(event).await.is_ok(),
    }
}

/// Writes `message` as one line of JSON, without the line's end: Codex's
/// `serialize_outgoing_message`. A message that cannot be written is logged and skipped.
fn serialize_outgoing_message(message: &JsonRpcMessage) -> Option<String> {
    serde_json::to_string(message)
        .inspect_err(|error| tracing::error!("Failed to serialize JSONRPCMessage: {error}"))
        .ok()
}

/// What the tests reach that the public entry points cannot produce. Only compiled with the
/// `test-api` feature.
#[cfg(feature = "test-api")]
pub mod test_api {
    use tokio::sync::mpsc;

    use super::{ConnectionId, QueuedOutgoingMessage, TransportEvent};
    use crate::frame::JsonRpcMessage;

    /// How every transport hands a message to the server, including when the server's channel and
    /// the connection's write queue are full.
    pub async fn enqueue_incoming_message(
        transport_event_tx: &mpsc::Sender<TransportEvent>,
        writer: &mpsc::Sender<QueuedOutgoingMessage>,
        connection_id: ConnectionId,
        message: JsonRpcMessage,
    ) -> bool {
        super::enqueue_incoming_message(transport_event_tx, writer, connection_id, message).await
    }
}
