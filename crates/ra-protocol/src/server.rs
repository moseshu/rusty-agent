//! The app server: reads what the transports deliver, answers the client's requests and routes the
//! server's messages back, as Codex's app-server does (`app-server/src/lib.rs`).
//!
//! The server runs two loops. One reads [`TransportEvent`]s: it keeps the state of each connection,
//! answers requests in the order they arrive, and routes the client's answers to the server's own
//! requests. The other writes the server's messages to the connections they are for. A message for
//! every connection reaches only those that have been initialized; a notification is withheld from
//! a connection that opted out of its method, and an experimental one from a connection that did
//! not opt into the experimental API. A connection the server can drop — a WebSocket — is dropped
//! when its write queue is full; stdio is waited on. The two loops share no state but the flags a
//! connection's handshake sets, as in Codex.
//!
//! A connection must open with `initialize`; see [`protocol`] for the handshake. The `thread/*`
//! and `turn/*` methods and notifications are added over the thread store as they are ported.
//!
//! While a turn runs the server asks the client in turn — approvals, dynamic tool calls, questions
//! for the user — with requests of its own, sent to the connections of the thread. The client's
//! response or error is routed back to whoever asked; a request whose turn moves on is withdrawn;
//! and once a request is answered or withdrawn the thread's connections are sent
//! `serverRequest/resolved`. A connection that attaches to the thread later is sent the requests
//! still waiting. What the server makes of each answer follows Codex's handlers, failing closed.
//! The turns that raise these requests arrive with `turn/start`.
//!
//! Codex's loop also drains running turns before a restart on a signal, recovers the threads of
//! its daemon, attaches newly created threads to every connection and reports the status of
//! remote control; none of it is ported yet.

// Nothing raises a server request until turns run in the server; the tests reach them through
// `test_api` meanwhile.
#[cfg_attr(not(feature = "test-api"), allow(dead_code))]
mod answers;
mod approvals;
mod outgoing;
mod processor;
pub mod protocol;
mod router;
mod thread_state;

pub use self::approvals::ReviewDecision;

use std::{
    collections::{HashMap, HashSet},
    io,
    path::Path,
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, Ordering},
    },
};

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use self::{
    outgoing::{OutgoingEnvelope, OutgoingMessageSender},
    processor::{ConnectionSessionState, MessageProcessor},
    router::{OutboundConnectionState, route_outgoing_envelope},
};
use crate::{
    absolute_path,
    frame::JsonRpcMessage,
    transport::{
        CHANNEL_CAPACITY, ConnectionId, ConnectionOrigin, QueuedOutgoingMessage, TransportEvent,
    },
};

/// What the reading loop tells the writing loop about connections: Codex's
/// `OutboundControlEvent`, without the graceful restart that disconnects every client.
enum OutboundControlEvent {
    /// A connection opened.
    Opened {
        connection_id: ConnectionId,
        writer: mpsc::Sender<QueuedOutgoingMessage>,
        disconnect_sender: Option<CancellationToken>,
        initialized: Arc<AtomicBool>,
        experimental_api_enabled: Arc<AtomicBool>,
        opted_out_notification_methods: Arc<RwLock<HashSet<String>>>,
    },
    /// A connection closed.
    Closed { connection_id: ConnectionId },
}

/// The reading loop's state of a connection: Codex's `ConnectionState`.
struct ConnectionState {
    outbound_initialized: Arc<AtomicBool>,
    outbound_experimental_api_enabled: Arc<AtomicBool>,
    outbound_opted_out_notification_methods: Arc<RwLock<HashSet<String>>>,
    session: Arc<ConnectionSessionState>,
}

impl ConnectionState {
    fn new(
        origin: ConnectionOrigin,
        outbound_initialized: Arc<AtomicBool>,
        outbound_experimental_api_enabled: Arc<AtomicBool>,
        outbound_opted_out_notification_methods: Arc<RwLock<HashSet<String>>>,
    ) -> Self {
        Self {
            outbound_initialized,
            outbound_experimental_api_enabled,
            outbound_opted_out_notification_methods,
            session: Arc::new(ConnectionSessionState::new(origin)),
        }
    }
}

/// The app server over the connections of one or more transports: the processor and outbound
/// router of Codex's `run_main_with_transport`.
///
/// Start the transports with the sending end of a channel of [`CHANNEL_CAPACITY`] and hand its
/// receiving end to [`run`](Self::run).
#[derive(Debug)]
pub struct AppServer {
    processor: Arc<MessageProcessor>,
    outgoing_rx: mpsc::Receiver<OutgoingEnvelope>,
}

impl AppServer {
    /// A server with no connections yet, using the host's configuration and state directory.
    ///
    /// The directory is returned as `codexHome` in every `initialize` response, preserving Codex's
    /// wire field and its meaning. The host supplies it instead of this library choosing a global
    /// home. The path is expanded and made absolute without accessing or creating the directory,
    /// as Codex's `AbsolutePathBuf::from_absolute_path` does.
    ///
    /// # Errors
    ///
    /// Fails if resolving a relative path cannot read the current directory.
    pub fn new(codex_home: impl AsRef<Path>) -> io::Result<Self> {
        let codex_home = absolute_path::from_absolute_path(codex_home.as_ref())?;
        let (outgoing_tx, outgoing_rx) = mpsc::channel::<OutgoingEnvelope>(CHANNEL_CAPACITY);
        let outgoing = Arc::new(OutgoingMessageSender::new(outgoing_tx));
        Ok(Self {
            processor: Arc::new(MessageProcessor::new(outgoing, codex_home)),
            outgoing_rx,
        })
    }

    /// Serves the connections `transport_event_rx` reports until every transport has gone or, as
    /// Codex does when serving stdio, the stdio connection closes.
    ///
    /// Returns once what has been routed to the connections is in their write queues; the
    /// transports write it out.
    pub async fn run(self, mut transport_event_rx: mpsc::Receiver<TransportEvent>) {
        let Self {
            processor,
            outgoing_rx,
        } = self;
        let (outbound_control_tx, outbound_control_rx) =
            mpsc::channel::<OutboundControlEvent>(CHANNEL_CAPACITY);

        let outbound_handle = tokio::spawn(run_outbound_router(outbound_control_rx, outgoing_rx));

        let mut connections = HashMap::<ConnectionId, ConnectionState>::new();
        let exit_reason = loop {
            let Some(event) = transport_event_rx.recv().await else {
                break "transport_channel_closed";
            };
            match event {
                TransportEvent::ConnectionOpened {
                    connection_id,
                    origin,
                    writer,
                    disconnect_sender,
                } => {
                    let outbound_initialized = Arc::new(AtomicBool::new(false));
                    let outbound_experimental_api_enabled = Arc::new(AtomicBool::new(false));
                    let outbound_opted_out_notification_methods =
                        Arc::new(RwLock::new(HashSet::new()));
                    if outbound_control_tx
                        .send(OutboundControlEvent::Opened {
                            connection_id,
                            writer,
                            disconnect_sender,
                            initialized: Arc::clone(&outbound_initialized),
                            experimental_api_enabled: Arc::clone(
                                &outbound_experimental_api_enabled,
                            ),
                            opted_out_notification_methods: Arc::clone(
                                &outbound_opted_out_notification_methods,
                            ),
                        })
                        .await
                        .is_err()
                    {
                        break "outbound_router_closed";
                    }
                    connections.insert(
                        connection_id,
                        ConnectionState::new(
                            origin,
                            outbound_initialized,
                            outbound_experimental_api_enabled,
                            outbound_opted_out_notification_methods,
                        ),
                    );
                }
                TransportEvent::ConnectionClosed { connection_id } => {
                    // A transport may report a connection closed twice, as stdio does when a
                    // signal follows the end of stdin.
                    let Some(connection_state) = connections.remove(&connection_id) else {
                        continue;
                    };
                    processor
                        .outgoing()
                        .thread_state()
                        .remove_connection(connection_id);
                    if outbound_control_tx
                        .send(OutboundControlEvent::Closed { connection_id })
                        .await
                        .is_err()
                    {
                        break "outbound_router_closed";
                    }
                    if connection_state.session.origin == ConnectionOrigin::Stdio {
                        break "stdio_connection_closed";
                    }
                }
                TransportEvent::IncomingMessage {
                    connection_id,
                    message,
                } => {
                    let Some(connection_state) = connections.get(&connection_id) else {
                        tracing::warn!("dropping message from unknown connection: {connection_id}");
                        continue;
                    };
                    handle_incoming_message(&processor, connection_id, connection_state, message)
                        .await;
                }
            }
        };

        tracing::info!(
            exit_reason,
            remaining_connection_count = connections.len(),
            "processor task exited"
        );
        drop(outbound_control_tx);
        let _ = outbound_handle.await;
    }
}

/// Writes the server's messages to their connections until the reading loop has gone: Codex's
/// outbound router task.
async fn run_outbound_router(
    mut outbound_control_rx: mpsc::Receiver<OutboundControlEvent>,
    mut outgoing_rx: mpsc::Receiver<OutgoingEnvelope>,
) {
    let mut outbound_connections = HashMap::<ConnectionId, OutboundConnectionState>::new();
    loop {
        tokio::select! {
            biased;
            event = outbound_control_rx.recv() => {
                let Some(event) = event else {
                    break;
                };
                match event {
                    OutboundControlEvent::Opened {
                        connection_id,
                        writer,
                        disconnect_sender,
                        initialized,
                        experimental_api_enabled,
                        opted_out_notification_methods,
                    } => {
                        outbound_connections.insert(
                            connection_id,
                            OutboundConnectionState::new(
                                writer,
                                initialized,
                                experimental_api_enabled,
                                opted_out_notification_methods,
                                disconnect_sender,
                            ),
                        );
                    }
                    OutboundControlEvent::Closed { connection_id } => {
                        outbound_connections.remove(&connection_id);
                    }
                }
            }
            envelope = outgoing_rx.recv() => {
                let Some(envelope) = envelope else {
                    break;
                };
                route_outgoing_envelope(&mut outbound_connections, envelope).await;
            }
        }
    }
    tracing::info!("outbound router task exited (channel closed)");
}

/// Handles one message from `connection_id`: Codex's handling of `TransportEvent::IncomingMessage`.
async fn handle_incoming_message(
    processor: &Arc<MessageProcessor>,
    connection_id: ConnectionId,
    connection_state: &ConnectionState,
    message: JsonRpcMessage,
) {
    match message {
        JsonRpcMessage::Request(request) => {
            let was_initialized = connection_state.session.initialized();
            processor
                .process_request(
                    connection_id,
                    request,
                    Arc::clone(&connection_state.session),
                )
                .await;
            // Codex copies these to the router after every request; they are only ever set by the
            // one `initialize` that succeeds, so copying them then is the same.
            if !was_initialized && connection_state.session.initialized() {
                if let Ok(mut opted_out_notification_methods) = connection_state
                    .outbound_opted_out_notification_methods
                    .write()
                {
                    *opted_out_notification_methods =
                        connection_state.session.opted_out_notification_methods();
                } else {
                    tracing::warn!("failed to update outbound opted-out notifications");
                }
                connection_state.outbound_experimental_api_enabled.store(
                    connection_state.session.experimental_api_enabled(),
                    Ordering::Release,
                );
                // Codex first sends the connection its configuration warnings and the status of
                // remote control, neither of which is ported, so the connection can now receive
                // what is sent to every connection.
                connection_state
                    .outbound_initialized
                    .store(true, Ordering::Release);
            }
        }
        JsonRpcMessage::Response(response) => {
            processor
                .outgoing()
                .notify_client_response(response.id(), response.result().clone());
        }
        JsonRpcMessage::Notification(notification) => {
            // As in Codex, no notification from a client is read; `initialized` among them.
            tracing::info!("<- notification: {notification:?}");
        }
        JsonRpcMessage::Error(error) => {
            processor
                .outgoing()
                .notify_client_error(error.id(), error.error().clone());
        }
    }
}

/// What the tests reach that the public entry points cannot produce. Only compiled with the
/// `test-api` feature.
#[cfg(feature = "test-api")]
pub mod test_api {
    use std::sync::Arc;

    use ra_core::session::SessionId;
    use serde_json::Value;
    use tokio::sync::oneshot;

    use super::{
        AppServer, answers,
        outgoing::{OutgoingMessageSender, ServerNotification, ThreadScopedOutgoingMessageSender},
        protocol::{
            DynamicToolCallResponse, PermissionsRequestApprovalResponse, ServerRequest,
            ServerRequestPayload, ToolRequestUserInputResponse,
        },
    };
    use crate::{
        frame::{RequestId, RequestResult},
        transport::ConnectionId,
    };

    pub use super::ReviewDecision;

    /// What the server receives for one of its requests: the client's answer, or nothing if the
    /// request was withdrawn without one.
    pub type ClientAnswer = Result<RequestResult, oneshot::error::RecvError>;

    /// Sends notifications from a server, which has none of its own yet.
    #[derive(Debug)]
    pub struct ServerNotifier {
        outgoing: Arc<OutgoingMessageSender>,
    }

    impl ServerNotifier {
        /// Sends from `server`, before or while it runs.
        #[must_use]
        pub fn new(server: &AppServer) -> Self {
            Self {
                outgoing: Arc::clone(server.processor.outgoing()),
            }
        }

        /// Sends a notification of `method` with `params`, marked experimental for
        /// `experimental_reason` if there is one, to each of `connection_ids`, or to every
        /// initialized connection when there are none.
        pub async fn send(
            &self,
            connection_ids: &[ConnectionId],
            method: &str,
            params: Value,
            experimental_reason: Option<&'static str>,
        ) {
            let mut notification = ServerNotification::new(method, params);
            if let Some(reason) = experimental_reason {
                notification = notification.experimental(reason);
            }
            if connection_ids.is_empty() {
                self.outgoing.send_server_notification(notification).await;
            } else {
                self.outgoing
                    .send_server_notification_to_connections(connection_ids, notification)
                    .await;
            }
        }
    }

    /// Sends a server's requests for one thread to the connections subscribed to it, as the turns
    /// of the thread will, which do not run in the server yet.
    #[derive(Debug)]
    pub struct ThreadRequester {
        outgoing: Arc<OutgoingMessageSender>,
        thread: ThreadScopedOutgoingMessageSender,
        thread_id: SessionId,
    }

    impl ThreadRequester {
        /// Sends from `server`, for the thread `thread_id`, to `connection_ids`.
        #[must_use]
        pub fn new(
            server: &AppServer,
            connection_ids: &[ConnectionId],
            thread_id: SessionId,
        ) -> Self {
            let outgoing = Arc::clone(server.processor.outgoing());
            for &connection_id in connection_ids {
                outgoing
                    .thread_state()
                    .add_connection_to_thread(&thread_id, connection_id);
            }
            Self {
                thread: ThreadScopedOutgoingMessageSender::new(
                    Arc::clone(&outgoing),
                    connection_ids.to_vec(),
                    thread_id.clone(),
                ),
                outgoing,
                thread_id,
            }
        }

        /// Sends `payload` and keeps it until it is answered or withdrawn. Returns its id and
        /// where its answer will arrive.
        pub async fn send_request(
            &self,
            payload: ServerRequestPayload,
        ) -> (RequestId, oneshot::Receiver<RequestResult>) {
            self.thread.send_request(payload).await
        }

        /// Waits for the answer to `request_id`, then sends `serverRequest/resolved`.
        pub async fn wait_for_answer(
            &self,
            request_id: RequestId,
            answer: oneshot::Receiver<RequestResult>,
        ) -> ClientAnswer {
            self.thread.wait_for_answer(request_id, answer).await
        }

        /// Withdraws the thread's requests, as a turn starting, completing or being aborted does.
        pub fn abort_pending_server_requests(&self) {
            self.thread.abort_pending_server_requests();
        }

        /// Sends `connection_id` the thread's requests still waiting, as attaching to the thread
        /// does.
        pub async fn replay_to_connection(&self, connection_id: ConnectionId) {
            self.outgoing
                .thread_state()
                .add_connection_to_thread(&self.thread_id, connection_id);
            self.outgoing
                .replay_requests_to_connection_for_thread(connection_id, &self.thread_id)
                .await;
        }

        /// Removes this connection's subscription, as `thread/unsubscribe` does.
        #[must_use]
        pub fn unsubscribe_connection(&self, connection_id: ConnectionId) -> bool {
            self.outgoing
                .thread_state()
                .unsubscribe_connection_from_thread(&self.thread_id, connection_id)
        }

        /// The thread's requests still waiting, in the order of their ids.
        #[must_use]
        pub fn pending_requests(&self) -> Vec<ServerRequest> {
            self.outgoing.pending_requests_for_thread(&self.thread_id)
        }
    }

    /// The decision a command approval's answer carries, or `None` if the request was withdrawn
    /// on a turn transition.
    #[must_use]
    pub fn command_execution_approval_decision(answer: ClientAnswer) -> Option<ReviewDecision> {
        answers::command_execution_approval_decision(answer)
    }

    /// The decision a file change approval's answer carries, or `None` if the request was
    /// withdrawn on a turn transition.
    #[must_use]
    pub fn file_change_approval_decision(answer: ClientAnswer) -> Option<ReviewDecision> {
        answers::file_change_approval_decision(answer)
    }

    /// The user's answers, or `None` if the request was withdrawn on a turn transition.
    #[must_use]
    pub fn request_user_input_response(
        answer: ClientAnswer,
    ) -> Option<ToolRequestUserInputResponse> {
        answers::request_user_input_response(answer)
    }

    /// The permissions granted, or `None` if the request was withdrawn on a turn transition.
    #[must_use]
    pub fn request_permissions_response(
        answer: ClientAnswer,
    ) -> Option<PermissionsRequestApprovalResponse> {
        answers::request_permissions_response(answer)
    }

    /// What the dynamic tool returned, or `None` if the request was withdrawn on a turn
    /// transition.
    #[must_use]
    pub fn dynamic_tool_call_response(answer: ClientAnswer) -> Option<DynamicToolCallResponse> {
        answers::dynamic_tool_call_response(answer)
    }
}
