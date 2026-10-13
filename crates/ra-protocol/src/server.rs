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
//! A connection must open with `initialize`; see [`protocol`] for the handshake. `thread/start`,
//! `thread/resume` and `thread/fork` load threads — prepared by the host's
//! [`ThreadFactory`] ([`AppServer::with_thread_factory`]) and recorded into the server's store
//! ([`AppServer::with_thread_store`]) — and subscribe the calling connection; `thread/read` and
//! `thread/loaded/list` read them. The turn methods — `turn/start`, `turn/steer`,
//! `turn/interrupt` and `thread/settings/update` — act on the threads the server has loaded, and
//! each loaded thread has a listener that tells its connections when its turns start and end and,
//! as they run, what they are made of: `item/started` and `item/completed` for the user's input,
//! the agent's messages and reasoning, and the tool calls the host shows
//! ([`AppServer::with_thread_tool_items`]). Every connection hears `thread/started` of a new
//! thread and `thread/status/changed` as a loaded thread's status changes. A client that comes back
//! attaches again with `thread/resume`, which answers with the thread's turns so far, and reads
//! more with `thread/turns/list`, both rebuilt from the thread's rollout.
//!
//! While a turn runs the server asks the client in turn — approvals, dynamic tool calls, questions
//! for the user — with requests of its own, sent to the connections of the thread. The client's
//! response or error is routed back to whoever asked; a request whose turn moves on is withdrawn;
//! and once a request is answered or withdrawn the thread's connections are sent
//! `serverRequest/resolved` — except for a dynamic tool call's, which Codex does not report. A
//! connection that attaches to the thread later is sent the requests still waiting. What the
//! server makes of each answer follows Codex's handlers, failing closed. Approvals, dynamic tool
//! calls and questions for the user are raised from the turns; permission requests are still to be
//! connected.
//!
//! # Lifecycle
//!
//! A turn's end ends only the turn: the thread stays loaded, the connection open, and what the
//! thread's tools left running — background terminals — keeps running, listed and stopped with
//! `thread/backgroundTerminals/*`. A connection leaves a thread with `thread/unsubscribe` or by
//! closing; a thread left with no subscribers and no active turn for the unload delay
//! ([`AppServer::with_thread_unload_delay`]) is shut down and every connection is told with
//! `thread/closed`. A closing connection's queued requests are dropped and its running ones waited
//! for before its subscriptions go; the server requests of its threads stay with the threads, for
//! the next client to answer.
//!
//! A server given [`AppServer::with_graceful_restart`] drains on `SIGINT`, `SIGTERM` or `SIGHUP`,
//! as Codex's does: requests that would start, steer or reconfigure turns are refused from then
//! on, and once the turns running have ended, the transports are told to stop and every client is
//! disconnected. A second `SIGINT` or `SIGTERM` stops it at once; `SIGHUP` never does. When the
//! server stops on its own — its transports gone, or its stdio client — it first cleans up after
//! the connections left and shuts its threads down.
//!
//! Codex's loop also recovers the threads of its daemon, attaches the threads its agents spawn to
//! every connection and reports the status of remote control; none of it is ported.

// Nothing raises permission requests yet; the tests reach their answers through `test_api`
// meanwhile.
#[cfg_attr(not(feature = "test-api"), allow(dead_code))]
mod answers;
mod approval_requests;
mod approvals;
mod background_terminals;
mod connection_cleanup;
mod connection_rpc_gate;
mod dynamic_tool_calls;
mod dynamic_tools;
mod image_url;
mod json_schema;
mod outgoing;
mod processor;
pub mod protocol;
mod request_serialization;
mod router;
mod settings_adapter;
mod thread_factory;
mod thread_history;
mod thread_listener;
mod thread_loading;
mod thread_management;
mod thread_manager;
mod thread_processor;
mod thread_state;
mod thread_status;
mod tool_items;
mod turn_admission;
mod turn_processor;
mod user_input_requests;

pub use self::approvals::ReviewDecision;
pub use self::background_terminals::{BackgroundTerminalInfo, ThreadBackgroundTerminals};
pub use self::settings_adapter::{PendingApproval, ThreadSettingsAdapter};
pub use self::thread_factory::{PreparedThread, ThreadFactory, ThreadFactoryRequest, ThreadOrigin};
pub use self::tool_items::{ThreadToolItemEvent, ThreadToolItems};

use std::{
    collections::{HashMap, HashSet},
    io,
    path::Path,
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use ra_session::store::ThreadStore;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use self::{
    connection_cleanup::ConnectionCleanupTasks,
    outgoing::{OutgoingEnvelope, OutgoingMessageSender},
    processor::{ConnectionSessionState, MessageProcessor},
    router::{OutboundConnectionState, route_outgoing_envelope},
    turn_admission::TurnAdmission,
};
use crate::{
    absolute_path,
    frame::JsonRpcMessage,
    transport::{
        CHANNEL_CAPACITY, ConnectionId, ConnectionOrigin, QueuedOutgoingMessage, TransportEvent,
    },
};

/// What the reading loop tells the writing loop about connections: Codex's
/// `OutboundControlEvent`.
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
    /// Disconnect every client that can be, as a drained restart does.
    DisconnectAll,
}

/// What a signal asks of a server set to restart gracefully: Codex's `ShutdownSignal`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ShutdownSignal {
    /// `SIGINT` or `SIGTERM`: drain, or stop at once if already draining.
    Forceable,
    /// `SIGHUP`: drain, however often it comes.
    GracefulOnly,
}

/// Where a server set to restart gracefully hears its signals from.
enum ShutdownSignals {
    /// The process's signals.
    Os,
    /// Signals the tests send.
    #[cfg(feature = "test-api")]
    Channel(mpsc::Receiver<ShutdownSignal>),
}

impl ShutdownSignals {
    /// Waits for the next signal: Codex's `shutdown_signal`.
    async fn next(&mut self) -> io::Result<ShutdownSignal> {
        match self {
            Self::Os => os_shutdown_signal().await,
            #[cfg(feature = "test-api")]
            Self::Channel(signals) => match signals.recv().await {
                Some(signal) => Ok(signal),
                None => std::future::pending().await,
            },
        }
    }
}

async fn os_shutdown_signal() -> io::Result<ShutdownSignal> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let mut term = signal(SignalKind::terminate())?;
        let mut hangup = signal(SignalKind::hangup())?;
        tokio::select! {
            ctrl_c_result = tokio::signal::ctrl_c() => ctrl_c_result.map(|()| ShutdownSignal::Forceable),
            _ = term.recv() => Ok(ShutdownSignal::Forceable),
            _ = hangup.recv() => Ok(ShutdownSignal::GracefulOnly),
        }
    }

    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await?;
        Ok(ShutdownSignal::Forceable)
    }
}

/// A server's graceful restart: the token that stops its transports, and its signals.
struct GracefulRestart {
    transport_shutdown: CancellationToken,
    signals: ShutdownSignals,
}

impl std::fmt::Debug for GracefulRestart {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GracefulRestart")
            .finish_non_exhaustive()
    }
}

/// Where a drain stands: Codex's `ShutdownState`.
#[derive(Default)]
struct ShutdownState {
    requested: bool,
    forced: bool,
    last_logged_running_turn_count: Option<usize>,
}

impl ShutdownState {
    const fn requested(&self) -> bool {
        self.requested
    }

    const fn forced(&self) -> bool {
        self.forced
    }

    fn on_signal(
        &mut self,
        signal: ShutdownSignal,
        connection_count: usize,
        running_turn_count: usize,
        turn_admission: &TurnAdmission,
    ) {
        if self.requested {
            if signal == ShutdownSignal::Forceable {
                self.forced = true;
            }
            return;
        }

        turn_admission.begin_drain();
        self.requested = true;
        self.last_logged_running_turn_count = None;
        tracing::info!(
            "received shutdown signal; entering graceful restart drain (connections={connection_count}, runningAssistantTurns={running_turn_count}, new client turns rejected)"
        );
    }

    /// Whether the drain may finish now.
    fn update(
        &mut self,
        running_turn_count: usize,
        active_admissions: usize,
        connection_count: usize,
    ) -> bool {
        if !self.requested {
            return false;
        }

        if self.forced || (running_turn_count == 0 && active_admissions == 0) {
            if self.forced {
                tracing::info!(
                    "received second shutdown signal; forcing restart with {running_turn_count} running assistant turn(s) and {connection_count} connection(s)"
                );
            } else {
                tracing::info!(
                    "shutdown signal restart: no assistant turns running; stopping acceptor and disconnecting {connection_count} connection(s)"
                );
            }
            return true;
        }

        if self.last_logged_running_turn_count != Some(running_turn_count) {
            tracing::info!(
                "shutdown signal restart: waiting for {running_turn_count} running assistant turn(s) and {active_admissions} admitted request(s) to finish"
            );
            self.last_logged_running_turn_count = Some(running_turn_count);
        }
        false
    }
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
    graceful_restart: Option<GracefulRestart>,
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
            graceful_restart: None,
        })
    }

    /// Reads threads' history from `store`, as Codex's server reads its thread store: what
    /// `thread/turns/list` rebuilds a thread's turns from. Without one, a thread's history cannot
    /// be read back.
    #[must_use]
    pub fn with_thread_store(self, store: Arc<dyn ThreadStore>) -> Self {
        self.processor.history.set_store(store);
        self
    }

    /// Prepares the threads `thread/start`, `thread/resume` and `thread/fork` load with `factory`,
    /// as Codex's server builds its sessions from its configuration (see [`ThreadFactory`]).
    /// Without one, those methods answer that they are not supported.
    #[must_use]
    pub fn with_thread_factory(self, factory: Arc<dyn ThreadFactory>) -> Self {
        self.processor.set_thread_factory(factory);
        self
    }

    /// Shows the tool calls of threads' turns as `tool_items` says, live and when their history is
    /// read back. Without it, a tool call shows no item, as a Codex function tool with no item of
    /// its own shows none.
    #[must_use]
    pub fn with_thread_tool_items(self, tool_items: Arc<dyn ThreadToolItems>) -> Self {
        self.processor.history.set_tool_items(tool_items);
        self
    }

    /// Unloads a thread once it has had no subscribers and no active turn for `delay`, instead of
    /// the default 60 seconds: Codex's `thread_unload_delay_secs`.
    ///
    /// Applies to the threads loaded after it is set. A delay too long to add to the current time
    /// keeps threads loaded.
    #[must_use]
    pub fn with_thread_unload_delay(self, delay: Duration) -> Self {
        self.processor.set_thread_unload_delay(delay);
        self
    }

    /// Drains for a restart on `SIGINT`, `SIGTERM` or `SIGHUP`, as Codex's server does when it
    /// installs its shutdown signal handler: turn work is refused from the first signal on, and once
    /// the turns running have ended `transport_shutdown` is cancelled, every client disconnected,
    /// and [`run`](Self::run) returns. A second `SIGINT` or `SIGTERM` does that at once.
    ///
    /// For servers of WebSocket connections — pass the token their acceptor stops on. Codex does
    /// not drain a stdio server, whose transport handles `SIGTERM` itself; do not set it there.
    /// Installing the handler takes over the process's handling of these signals.
    #[must_use]
    pub fn with_graceful_restart(mut self, transport_shutdown: CancellationToken) -> Self {
        self.graceful_restart = Some(GracefulRestart {
            transport_shutdown,
            signals: ShutdownSignals::Os,
        });
        self
    }

    /// Serves the connections `transport_event_rx` reports until every transport has gone, the
    /// stdio connection closes — as Codex's server does when it serves stdio — or a graceful
    /// restart has drained.
    ///
    /// Unless a second signal forced the stop, it then cleans up after the connections left and
    /// shuts the loaded threads down, interrupting any turn still running. Returns once what has
    /// been routed to the connections is in their write queues; the transports write it out.
    #[allow(clippy::too_many_lines)] // Codex's processor loop: one select over every source.
    pub async fn run(self, mut transport_event_rx: mpsc::Receiver<TransportEvent>) {
        let Self {
            processor,
            outgoing_rx,
            graceful_restart,
        } = self;
        let (outbound_control_tx, outbound_control_rx) =
            mpsc::channel::<OutboundControlEvent>(CHANNEL_CAPACITY);

        let outbound_handle = tokio::spawn(run_outbound_router(outbound_control_rx, outgoing_rx));

        let (transport_shutdown, mut signals) = match graceful_restart {
            Some(GracefulRestart {
                transport_shutdown,
                signals,
            }) => (Some(transport_shutdown), Some(signals)),
            None => (None, None),
        };
        let mut running_turn_count_rx = processor
            .outgoing()
            .thread_state()
            .subscribe_running_turn_count();
        let mut active_admissions_rx = processor.turn_admission.subscribe_active();
        let mut shutdown_state = ShutdownState::default();
        let mut connections = HashMap::<ConnectionId, ConnectionState>::new();
        let mut connection_cleanup_tasks = ConnectionCleanupTasks::new();
        let exit_reason = loop {
            // Admissions are read first: a request can start a turn before it lets its permit go,
            // and the drain must see that turn.
            let active_admissions = *active_admissions_rx.borrow_and_update();
            let running_turn_count = running_turn_count(&processor, &mut running_turn_count_rx);
            if shutdown_state.update(running_turn_count, active_admissions, connections.len()) {
                if let Some(transport_shutdown) = &transport_shutdown {
                    transport_shutdown.cancel();
                }
                let _ = outbound_control_tx
                    .send(OutboundControlEvent::DisconnectAll)
                    .await;
                break "shutdown_requested";
            }

            tokio::select! {
                signal = next_signal(&mut signals), if !shutdown_state.forced() => {
                    let signal = match signal {
                        Ok(signal) => signal,
                        Err(err) => {
                            tracing::warn!("failed to listen for shutdown signal during graceful restart drain: {err}");
                            signals = None;
                            continue;
                        }
                    };
                    shutdown_state.on_signal(
                        signal,
                        connections.len(),
                        running_turn_count,
                        &processor.turn_admission,
                    );
                }
                changed = running_turn_count_rx.changed(), if shutdown_state.requested() => {
                    if changed.is_err() {
                        tracing::warn!("running-turn watcher closed during graceful restart drain");
                    }
                }
                changed = active_admissions_rx.changed(), if shutdown_state.requested() => {
                    if changed.is_err() {
                        tracing::warn!("turn admission watcher closed during graceful restart drain");
                    }
                }
                event = transport_event_rx.recv() => {
                    let Some(event) = event else {
                        break "transport_channel_closed";
                    };
                    match event {
                        TransportEvent::ConnectionOpened {
                            connection_id,
                            origin,
                            writer,
                            disconnect_sender,
                        } => {
                            let Some(connection_state) = open_connection(
                                &outbound_control_tx,
                                connection_id,
                                origin,
                                writer,
                                disconnect_sender,
                            )
                            .await
                            else {
                                break "outbound_router_closed";
                            };
                            connections.insert(connection_id, connection_state);
                        }
                        TransportEvent::ConnectionClosed { connection_id } => {
                            // A transport may report a connection closed twice, as stdio does when
                            // a signal follows the end of stdin.
                            let Some(connection_state) = connections.remove(&connection_id) else {
                                continue;
                            };
                            let stdio_closed =
                                connection_state.session.origin == ConnectionOrigin::Stdio;
                            connection_state.session.rpc_gate.close().await;
                            let outbound_closed = outbound_control_tx
                                .send(OutboundControlEvent::Closed { connection_id })
                                .await
                                .is_ok();
                            let processor = Arc::clone(&processor);
                            connection_cleanup_tasks.spawn(async move {
                                processor
                                    .connection_closed(connection_id, &connection_state.session)
                                    .await;
                            });
                            if !outbound_closed {
                                break "outbound_router_closed";
                            }
                            if stdio_closed {
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
                }
                () = connection_cleanup_tasks.reap_next() => {}
            }
        };

        if shutdown_state.forced() {
            connection_cleanup_tasks.abort();
        } else {
            futures::future::join_all(connections.iter().map(
                |(&connection_id, connection_state)| {
                    processor.connection_closed(connection_id, &connection_state.session)
                },
            ))
            .await;
            connection_cleanup_tasks.drain().await;
            processor.shutdown_threads().await;
        }
        tracing::info!(
            exit_reason,
            remaining_connection_count = connections.len(),
            shutdown_forced = shutdown_state.forced(),
            "processor task exited"
        );
        drop(outbound_control_tx);
        let _ = outbound_handle.await;
    }
}

/// Tells the writing loop of a connection that opened, and returns the reading loop's state of it;
/// `None` if the writing loop has gone.
async fn open_connection(
    outbound_control_tx: &mpsc::Sender<OutboundControlEvent>,
    connection_id: ConnectionId,
    origin: ConnectionOrigin,
    writer: mpsc::Sender<QueuedOutgoingMessage>,
    disconnect_sender: Option<CancellationToken>,
) -> Option<ConnectionState> {
    let outbound_initialized = Arc::new(AtomicBool::new(false));
    let outbound_experimental_api_enabled = Arc::new(AtomicBool::new(false));
    let outbound_opted_out_notification_methods = Arc::new(RwLock::new(HashSet::new()));
    outbound_control_tx
        .send(OutboundControlEvent::Opened {
            connection_id,
            writer,
            disconnect_sender,
            initialized: Arc::clone(&outbound_initialized),
            experimental_api_enabled: Arc::clone(&outbound_experimental_api_enabled),
            opted_out_notification_methods: Arc::clone(&outbound_opted_out_notification_methods),
        })
        .await
        .ok()?;
    Some(ConnectionState::new(
        origin,
        outbound_initialized,
        outbound_experimental_api_enabled,
        outbound_opted_out_notification_methods,
    ))
}

/// Waits for the next signal of a server set to restart gracefully; never returns for one that
/// is not.
async fn next_signal(signals: &mut Option<ShutdownSignals>) -> io::Result<ShutdownSignal> {
    match signals {
        Some(signals) => signals.next().await,
        None => std::future::pending().await,
    }
}

/// How many threads are running a turn: those whose listener has heard it start and not yet sent
/// its end, or — when there are none — whether any thread has started one its listener has not
/// heard of yet. Codex counts the first alone, so a turn that has just started can be missed.
fn running_turn_count(
    processor: &MessageProcessor,
    running_turn_count_rx: &mut tokio::sync::watch::Receiver<usize>,
) -> usize {
    let heard = *running_turn_count_rx.borrow_and_update();
    if heard == 0 && processor.thread_manager().any_running() {
        1
    } else {
        heard
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
                    OutboundControlEvent::DisconnectAll => {
                        // What was sent before the drain finished — the answer or the turn's end
                        // that let it finish among them — reaches the clients first. Codex's router
                        // takes the disconnect ahead of them.
                        while let Ok(envelope) = outgoing_rx.try_recv() {
                            route_outgoing_envelope(&mut outbound_connections, envelope).await;
                        }
                        tracing::info!(
                            "disconnecting {} outbound websocket connection(s) for graceful restart",
                            outbound_connections.len()
                        );
                        for connection_state in outbound_connections.values() {
                            connection_state.request_disconnect();
                        }
                        outbound_connections.clear();
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
    use tokio::sync::{mpsc, oneshot};
    use tokio_util::sync::CancellationToken;

    use ra_runtime::thread::AgentThread;

    use std::future::Future;

    use super::{
        AppServer, GracefulRestart, ShutdownSignal, ShutdownSignals, answers,
        connection_rpc_gate::ConnectionRpcGate,
        outgoing::{OutgoingMessageSender, ServerNotification, ThreadScopedOutgoingMessageSender},
        processor::MessageProcessor,
        protocol::{
            DynamicToolCallResponse, DynamicToolSpec, PermissionsRequestApprovalResponse,
            ServerRequest, ServerRequestPayload, ToolRequestUserInputResponse,
        },
        thread_listener::ensure_listener_task_running,
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

    /// Loads threads into a server without a thread factory, as the thread methods load them
    /// through one. A thread loaded this way is taken to keep no rollout the server knows of.
    #[derive(Debug)]
    pub struct ThreadLoader {
        processor: Arc<MessageProcessor>,
    }

    impl ThreadLoader {
        /// Loads into `server`, before or while it runs.
        #[must_use]
        pub fn new(server: &AppServer) -> Self {
            Self {
                processor: Arc::clone(&server.processor),
            }
        }

        /// Loads `thread`, subscribes `connection_ids` to it, and starts its listener, so its turn
        /// methods reach it and its connections hear of its turns. Must be called within a Tokio
        /// runtime, on which the listener runs.
        pub async fn load(&self, thread: AgentThread, connection_ids: &[ConnectionId]) {
            self.load_with(thread, connection_ids, None, None).await;
        }

        /// Installs the product's explicit policy adapter alongside its live thread.
        pub async fn load_with_adapter(
            &self,
            thread: AgentThread,
            connection_ids: &[ConnectionId],
            adapter: Option<Arc<dyn super::ThreadSettingsAdapter>>,
        ) {
            self.load_with(thread, connection_ids, adapter, None).await;
        }

        /// Hands over the commands the thread's tools leave running alongside the thread.
        pub async fn load_with_terminals(
            &self,
            thread: AgentThread,
            connection_ids: &[ConnectionId],
            terminals: Arc<dyn super::ThreadBackgroundTerminals>,
        ) {
            self.load_with(thread, connection_ids, None, Some(terminals))
                .await;
        }

        /// Subscribes `connection_id` to the loaded thread `thread_id`, as `thread/resume` does
        /// when it attaches a connection to a thread, whether or not the connection is live.
        pub fn subscribe(&self, thread_id: &SessionId, connection_id: ConnectionId) {
            self.processor
                .outgoing()
                .thread_state()
                .add_connection_to_thread(thread_id, connection_id);
        }

        async fn load_with(
            &self,
            thread: AgentThread,
            connection_ids: &[ConnectionId],
            adapter: Option<Arc<dyn super::ThreadSettingsAdapter>>,
            terminals: Option<Arc<dyn super::ThreadBackgroundTerminals>>,
        ) {
            let outgoing = self.processor.outgoing();
            // The thread's state exists before its listener watches it, subscribers or not.
            let _ = outgoing.thread_state().thread_state(thread.thread_id());
            for &connection_id in connection_ids {
                outgoing
                    .thread_state()
                    .add_connection_to_thread(thread.thread_id(), connection_id);
            }
            let loaded = self.processor.thread_manager().add_thread(
                thread,
                super::thread_manager::LoadedThreadInfo::unknown(),
                adapter,
                terminals,
                Arc::default(),
            );
            if let Err(error) =
                ensure_listener_task_running(&self.processor.listener_context(), &loaded).await
            {
                tracing::warn!("failed to attach listener for thread: {}", error.message());
            }
        }
    }

    /// A connection's request gate, reached on its own as Codex's unit tests reach it.
    #[derive(Debug, Default)]
    pub struct RpcGate(ConnectionRpcGate);

    impl RpcGate {
        /// An open gate.
        #[must_use]
        pub fn new() -> Self {
            Self::default()
        }

        /// Runs `future` if the gate is open, counting it until it finishes; drops it unpolled if
        /// not.
        pub async fn run(&self, future: impl Future<Output = ()>) {
            self.0.run(future).await;
        }

        /// Lets no more handlers start.
        pub async fn close(&self) {
            self.0.close().await;
        }

        /// Closes the gate and waits for the handlers running to finish.
        pub async fn shutdown(&self) {
            self.0.shutdown().await;
        }

        /// Whether handlers may still start.
        pub async fn is_accepting(&self) -> bool {
            self.0.is_accepting().await
        }

        /// How many handlers are running.
        #[must_use]
        pub fn inflight_count(&self) -> usize {
            self.0.inflight_count()
        }
    }

    /// Sends a server the signals a graceful restart listens for, in place of the process's.
    #[derive(Debug, Clone)]
    pub struct RestartSignals {
        signals: mpsc::Sender<ShutdownSignal>,
    }

    impl RestartSignals {
        /// Sets `server` to restart gracefully, as [`AppServer::with_graceful_restart`] does, on
        /// the signals sent through the returned handle.
        #[must_use]
        pub fn install(
            server: AppServer,
            transport_shutdown: CancellationToken,
        ) -> (AppServer, Self) {
            let (signals, receiver) = mpsc::channel(8);
            let mut server = server;
            server.graceful_restart = Some(GracefulRestart {
                transport_shutdown,
                signals: ShutdownSignals::Channel(receiver),
            });
            (server, Self { signals })
        }

        /// Sends `SIGINT` or `SIGTERM`. Returns whether the server was still listening.
        pub async fn forceable(&self) -> bool {
            self.signals.send(ShutdownSignal::Forceable).await.is_ok()
        }

        /// Sends `SIGHUP`. Returns whether the server was still listening.
        pub async fn graceful_only(&self) -> bool {
            self.signals
                .send(ShutdownSignal::GracefulOnly)
                .await
                .is_ok()
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

    /// A tool input schema as read into the schema subset, as JSON: Codex's
    /// `parse_tool_input_schema`.
    ///
    /// # Errors
    ///
    /// Returns the reason a schema the subset cannot read is refused.
    pub fn parse_tool_input_schema(input_schema: &Value) -> Result<Value, String> {
        super::json_schema::parse_tool_input_schema(input_schema)
            .and_then(serde_json::to_value)
            .map_err(|error| error.to_string())
    }

    /// [`parse_tool_input_schema`] compacting a schema over `max_bytes` rather than Codex's
    /// default size.
    ///
    /// # Errors
    ///
    /// Returns the reason a schema the subset cannot read is refused.
    pub fn parse_tool_input_schema_with_max_bytes(
        input_schema: &Value,
        max_bytes: usize,
    ) -> Result<Value, String> {
        super::json_schema::parse_tool_input_schema_with_max_bytes(input_schema, max_bytes)
            .and_then(serde_json::to_value)
            .map_err(|error| error.to_string())
    }

    /// [`parse_tool_input_schema`] without compacting a large schema: Codex's
    /// `parse_tool_input_schema_without_compaction`.
    ///
    /// # Errors
    ///
    /// Returns the reason a schema the subset cannot read is refused.
    pub fn parse_tool_input_schema_without_compaction(
        input_schema: &Value,
    ) -> Result<Value, String> {
        super::json_schema::parse_tool_input_schema_without_compaction(input_schema)
            .and_then(serde_json::to_value)
            .map_err(|error| error.to_string())
    }

    /// Codex's `validate_dynamic_tools`.
    ///
    /// # Errors
    ///
    /// Returns Codex's error for the first tool that fails a check.
    pub fn validate_dynamic_tools(tools: &[DynamicToolSpec]) -> Result<(), String> {
        super::dynamic_tools::validate_dynamic_tools(tools)
    }
}
