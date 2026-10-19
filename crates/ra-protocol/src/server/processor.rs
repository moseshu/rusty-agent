// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! What the server makes of a client's requests: Codex's `MessageProcessor`, `ConnectionSessionState`
//! and `InitializeRequestProcessor` (`app-server/src/message_processor.rs`,
//! `app-server/src/request_processors/initialize_processor.rs`).
//!
//! A request is first read as a [`ClientRequest`]; one that is not — an unknown method, or
//! parameters that do not fit — is refused as an invalid request. `initialize` is handled at once,
//! in the order requests arrive, and succeeds once per connection. Any other method is refused
//! with `Not initialized` until it has, and an experimental one is refused unless the connection
//! opted into the experimental API; a method that passes runs on a task of its own, so a slow one
//! does not hold up the next request.
//!
//! A method that changes a thread runs after the requests for the same thread that arrived before
//! it, whichever connection sent them (see [`super::request_serialization`]). Every method runs
//! through its connection's gate: once the connection has closed, what it queued no longer starts,
//! and its cleanup waits — for at most thirty seconds — for what had started before the
//! connection's subscriptions are dropped. A method that starts, steers or reconfigures turns
//! holds a permit from when it arrives until it has been handled; once the server drains for a
//! restart, no new permit is given (see [`super::turn_admission`]), and a turn that waited in its
//! thread's queue checks again when its turn comes.
//!
//! Codex's `initialize` also does its account and product work: it selects how its gateway logs
//! in, makes the client's name the `originator` and `User-Agent` suffix of every request to its
//! backend, enables user verification for its own desktop and terminal clients, reports the
//! connection to its analytics, and registers its Windows desktop install. None of this is ported;
//! the agent's own `User-Agent` stays as it is whoever connects.

use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{Arc, OnceLock, PoisonError, RwLock},
    time::Duration,
};

use ra_core::session::SessionId;
use serde::Serialize;
use tokio::{sync::Mutex, time::timeout};

use super::{
    SessionSource,
    connection_rpc_gate::ConnectionRpcGate,
    outgoing::{ConnectionRequestId, OutgoingMessageSender},
    protocol::{
        ClientRequest, InitializeParams, InitializeResponse, MockExperimentalMethodResponse,
        experimental_required_message,
    },
    request_serialization::{
        QueuedInitializedRequest, RequestSerializationQueueKey, RequestSerializationQueues,
    },
    thread_factory::ThreadFactory,
    thread_history::HistorySources,
    thread_listener::ListenerTaskContext,
    thread_loading::ThreadMethodContext,
    thread_manager::ThreadManager,
    thread_processor::ThreadRequestProcessor,
    thread_status::ThreadWatchManager,
    turn_admission::TurnAdmission,
    turn_processor::{TurnMethodResult, TurnRequestProcessor},
};
use crate::{
    frame::{JsonRpcErrorObject, JsonRpcRequest},
    transport::{ConnectionId, ConnectionOrigin},
};

/// The `User-Agent` the agent identifies itself with: the one its model adapters send.
const USER_AGENT: &str = concat!("rusty-agent/", env!("CARGO_PKG_VERSION"));

/// How long a closed connection's cleanup waits for its running handlers: Codex's
/// `CONNECTION_RPC_DRAIN_TIMEOUT`.
const CONNECTION_RPC_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a thread with no subscribers and no active turn stays loaded by default: Codex's
/// `thread_unload_delay_secs` default.
pub(crate) const DEFAULT_THREAD_UNLOAD_DELAY: Duration = Duration::from_secs(60);

/// What the server knows of one connection: Codex's `ConnectionSessionState`.
#[derive(Debug)]
pub(crate) struct ConnectionSessionState {
    pub(crate) origin: ConnectionOrigin,
    pub(crate) rpc_gate: Arc<ConnectionRpcGate>,
    initialized: OnceLock<InitializedConnectionSessionState>,
}

/// What a connection declared in its `initialize`: Codex's `InitializedConnectionSessionState`.
///
/// Codex also keeps whether the client takes attestation requests and its MCP extensions; the
/// first belongs to its account machinery, the second comes with the MCP methods.
#[derive(Debug)]
struct InitializedConnectionSessionState {
    experimental_api_enabled: bool,
    opted_out_notification_methods: HashSet<String>,
    // Codex hands the client's name and version to the thread methods, which are not ported yet.
    #[allow(dead_code)]
    app_server_client_name: String,
    #[allow(dead_code)]
    client_version: String,
}

impl ConnectionSessionState {
    pub(crate) fn new(origin: ConnectionOrigin) -> Self {
        Self {
            origin,
            rpc_gate: Arc::new(ConnectionRpcGate::new()),
            initialized: OnceLock::new(),
        }
    }

    pub(crate) fn initialized(&self) -> bool {
        self.initialized.get().is_some()
    }

    pub(crate) fn experimental_api_enabled(&self) -> bool {
        self.initialized
            .get()
            .is_some_and(|session| session.experimental_api_enabled)
    }

    pub(crate) fn opted_out_notification_methods(&self) -> HashSet<String> {
        self.initialized
            .get()
            .map(|session| session.opted_out_notification_methods.clone())
            .unwrap_or_default()
    }

    fn initialize(&self, session: InitializedConnectionSessionState) -> Result<(), ()> {
        self.initialized.set(session).map_err(|_| ())
    }
}

/// Where the host's thread factory is kept once given.
#[derive(Default)]
struct ThreadFactorySlot(std::sync::RwLock<Option<Arc<dyn ThreadFactory>>>);

impl std::fmt::Debug for ThreadFactorySlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let set = self
            .0
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some();
        f.debug_tuple("ThreadFactorySlot").field(&set).finish()
    }
}

/// Handles the requests of every connection: Codex's `MessageProcessor`, with the methods ported
/// so far.
#[derive(Debug)]
pub(crate) struct MessageProcessor {
    outgoing: Arc<OutgoingMessageSender>,
    pub(crate) mcp: super::mcp::McpControlSlot,
    codex_home: PathBuf,
    thread_manager: Arc<ThreadManager>,
    turn_processor: TurnRequestProcessor,
    thread_processor: ThreadRequestProcessor,
    request_serialization_queues: RequestSerializationQueues,
    pub(crate) turn_admission: TurnAdmission,
    pending_thread_unloads: Arc<Mutex<HashSet<SessionId>>>,
    thread_unload_delay: std::sync::Mutex<Duration>,
    /// Where threads' history is read from, and how their tools' calls show.
    pub(crate) history: Arc<HistorySources>,
    /// What prepares the threads the thread methods load, if the host gave one.
    thread_factory: ThreadFactorySlot,
    session_source: RwLock<SessionSource>,
    /// The statuses of the loaded threads.
    thread_watch: Arc<ThreadWatchManager>,
}

impl MessageProcessor {
    pub(crate) fn new(outgoing: Arc<OutgoingMessageSender>, codex_home: PathBuf) -> Self {
        let thread_manager = Arc::new(ThreadManager::default());
        let pending_thread_unloads = Arc::new(Mutex::new(HashSet::new()));
        let thread_watch = Arc::new(ThreadWatchManager::new(Arc::clone(&outgoing)));
        Self {
            turn_processor: TurnRequestProcessor::new(
                Arc::clone(&thread_manager),
                Arc::clone(&outgoing),
            ),
            thread_processor: ThreadRequestProcessor::new(
                Arc::clone(&thread_manager),
                Arc::clone(&outgoing),
                Arc::clone(&pending_thread_unloads),
                Arc::clone(&thread_watch),
            ),
            outgoing,
            mcp: super::mcp::McpControlSlot::default(),
            codex_home,
            thread_manager,
            request_serialization_queues: RequestSerializationQueues::default(),
            turn_admission: TurnAdmission::default(),
            pending_thread_unloads,
            thread_unload_delay: std::sync::Mutex::new(DEFAULT_THREAD_UNLOAD_DELAY),
            history: Arc::default(),
            thread_factory: ThreadFactorySlot::default(),
            session_source: RwLock::new(SessionSource::default()),
            thread_watch,
        }
    }

    /// Sets the server-level session source used by newly started and forked threads.
    pub(crate) fn set_session_source(&self, session_source: SessionSource) {
        *self
            .session_source
            .write()
            .unwrap_or_else(PoisonError::into_inner) = session_source;
    }

    /// Prepares the threads the thread methods load with `factory`.
    pub(crate) fn set_thread_factory(&self, factory: Arc<dyn ThreadFactory>) {
        *self
            .thread_factory
            .0
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Some(factory);
    }

    fn thread_method_context(&self) -> ThreadMethodContext<'_> {
        ThreadMethodContext {
            history: &self.history,
            factory: self
                .thread_factory
                .0
                .read()
                .unwrap_or_else(PoisonError::into_inner)
                .clone(),
            listener: self.listener_context(),
            session_source: self
                .session_source
                .read()
                .unwrap_or_else(PoisonError::into_inner)
                .clone(),
        }
    }

    /// Sets how long a thread with no subscribers and no active turn stays loaded, for the
    /// listeners started from now on.
    pub(crate) fn set_thread_unload_delay(&self, delay: Duration) {
        *self
            .thread_unload_delay
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = delay;
    }

    /// What a thread's listener works with.
    #[cfg_attr(not(feature = "test-api"), allow(dead_code))]
    pub(crate) fn listener_context(&self) -> ListenerTaskContext {
        ListenerTaskContext {
            outgoing: Arc::clone(&self.outgoing),
            history: Arc::clone(&self.history),
            thread_manager: Arc::clone(&self.thread_manager),
            pending_thread_unloads: Arc::clone(&self.pending_thread_unloads),
            thread_watch: Arc::clone(&self.thread_watch),
            thread_unload_delay: *self
                .thread_unload_delay
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        }
    }

    /// Cleans up after a closed connection: Codex's `connection_closed`. Its queued requests are
    /// dropped, its running handlers waited for, and then its subscriptions removed.
    pub(crate) async fn connection_closed(
        &self,
        connection_id: ConnectionId,
        session: &ConnectionSessionState,
    ) {
        session.rpc_gate.close().await;
        self.request_serialization_queues.discard_closed().await;
        if timeout(CONNECTION_RPC_DRAIN_TIMEOUT, session.rpc_gate.shutdown())
            .await
            .is_err()
        {
            tracing::warn!(
                %connection_id,
                timeout_seconds = CONNECTION_RPC_DRAIN_TIMEOUT.as_secs(),
                "timed out waiting for connection RPCs to drain"
            );
        }
        self.thread_processor.connection_closed(connection_id).await;
    }

    /// Shuts every loaded thread down as the server stops: Codex's `shutdown_threads`.
    pub(crate) async fn shutdown_threads(&self) {
        self.thread_processor.shutdown_threads().await;
    }

    pub(crate) fn outgoing(&self) -> &Arc<OutgoingMessageSender> {
        &self.outgoing
    }

    #[cfg_attr(not(feature = "test-api"), allow(dead_code))]
    pub(crate) fn thread_manager(&self) -> &Arc<ThreadManager> {
        &self.thread_manager
    }

    /// Handles a request from `connection_id`, answering it with an error if it fails: Codex's
    /// `process_request`. Returns once `initialize` has been answered, or once any other method has
    /// been admitted and started.
    pub(crate) async fn process_request(
        self: &Arc<Self>,
        connection_id: ConnectionId,
        request: JsonRpcRequest,
        session: Arc<ConnectionSessionState>,
    ) {
        tracing::trace!(
            %connection_id,
            request_id = %request.id(),
            "app-server request: {}",
            request.method()
        );
        let request_id = ConnectionRequestId {
            connection_id,
            request_id: request.id().clone(),
        };
        let result = match deserialize_client_request(request) {
            Ok(client_request) => {
                self.handle_client_request(request_id.clone(), client_request, session)
                    .await
            }
            Err(error) => Err(error),
        };
        if let Err(error) = result {
            self.outgoing.send_error(request_id, error).await;
        }
    }

    async fn handle_client_request(
        self: &Arc<Self>,
        request_id: ConnectionRequestId,
        client_request: ClientRequest,
        session: Arc<ConnectionSessionState>,
    ) -> Result<(), JsonRpcErrorObject> {
        if let ClientRequest::Initialize { params, .. } = client_request {
            return self.initialize(request_id, params, &session).await;
        }
        self.dispatch_initialized_client_request(request_id, client_request, session)
            .await
    }

    /// Codex's `InitializeRequestProcessor::initialize`, without its account and product work.
    async fn initialize(
        &self,
        request_id: ConnectionRequestId,
        params: InitializeParams,
        session: &ConnectionSessionState,
    ) -> Result<(), JsonRpcErrorObject> {
        if session.initialized() {
            return Err(JsonRpcErrorObject::invalid_request("Already initialized"));
        }

        // Codex notes that the experimental API is opted into per connection, so connections
        // sharing a thread may see it differently, and that it may become a choice the first
        // connection makes for the whole server; it is per connection there as here.
        let (client_info, capabilities) = params.into_parts();
        let (experimental_api_enabled, opt_out_notification_methods) =
            capabilities.unwrap_or_default().into_parts();
        let (name, version) = client_info.into_name_and_version();
        // Codex sends the name in the headers of its requests to OpenAI. It is not sent here, but
        // the check is part of the handshake a client sees.
        if http::HeaderValue::from_str(&name).is_err() {
            return Err(JsonRpcErrorObject::invalid_request(format!(
                "Invalid clientInfo.name: '{name}'. Must be a valid HTTP header value."
            )));
        }
        if session
            .initialize(InitializedConnectionSessionState {
                experimental_api_enabled,
                opted_out_notification_methods: opt_out_notification_methods.into_iter().collect(),
                app_server_client_name: name,
                client_version: version,
            })
            .is_err()
        {
            return Err(JsonRpcErrorObject::invalid_request("Already initialized"));
        }

        self.outgoing
            .thread_state()
            .connection_initialized(request_id.connection_id);
        self.send_response(
            request_id,
            InitializeResponse::new(USER_AGENT, self.codex_home.clone()),
        )
        .await;
        Ok(())
    }

    /// Admits a method other than `initialize` and starts it: Codex's
    /// `dispatch_initialized_client_request`.
    async fn dispatch_initialized_client_request(
        self: &Arc<Self>,
        request_id: ConnectionRequestId,
        client_request: ClientRequest,
        session: Arc<ConnectionSessionState>,
    ) -> Result<(), JsonRpcErrorObject> {
        if !session.initialized() {
            return Err(JsonRpcErrorObject::invalid_request("Not initialized"));
        }

        if let Some(reason) = client_request.experimental_reason()
            && !session.experimental_api_enabled()
        {
            return Err(JsonRpcErrorObject::invalid_request(
                experimental_required_message(reason),
            ));
        }

        let (turn_permit, recheck_turn_admission) = match &client_request {
            ClientRequest::ThreadSettingsUpdate { .. }
            | ClientRequest::TurnSettingsUpdate { .. } => {
                (Some(self.turn_admission.admit()?), false)
            }
            ClientRequest::TurnStart { .. } | ClientRequest::TurnSteer { .. } => {
                (Some(self.turn_admission.admit()?), true)
            }
            _ => (None, false),
        };

        let serialization_key = client_request
            .serialization_scope()
            .map(RequestSerializationQueueKey::from);
        let error_request_id = request_id.clone();
        let processor = Arc::clone(self);
        let rpc_gate = Arc::clone(&session.rpc_gate);
        let request = QueuedInitializedRequest::new(Arc::clone(&session.rpc_gate), async move {
            let _turn_permit = turn_permit;
            // Settings changes admitted before the drain finish normally; turn work still waiting
            // in its thread's queue must observe the closed admission.
            if recheck_turn_admission && let Err(error) = processor.turn_admission.admit() {
                processor.outgoing.send_error(error_request_id, error).await;
                return;
            }
            processor
                .handle_initialized_client_request(request_id, client_request, rpc_gate)
                .await;
        });

        if let Some(key) = serialization_key {
            self.request_serialization_queues
                .enqueue(key, request)
                .await;
        } else {
            tokio::spawn(request.run());
        }
        Ok(())
    }

    #[allow(clippy::too_many_lines)] // One arm per method, as Codex's dispatch has.
    async fn handle_initialized_client_request(
        &self,
        request_id: ConnectionRequestId,
        client_request: ClientRequest,
        rpc_gate: Arc<ConnectionRpcGate>,
    ) {
        match client_request {
            ClientRequest::McpServerStatusList { params, .. } => {
                let result = match self.mcp.get() {
                    Ok(control) => super::mcp::prepare_list(control.as_ref(), params),
                    Err(error) => Err(error),
                };
                match result {
                    Ok(inventory) => {
                        let outgoing = Arc::clone(&self.outgoing);
                        rpc_gate.spawn_admitted(async move {
                            outgoing
                                .send_serialized_response(request_id, inventory.await)
                                .await;
                        });
                    }
                    Err(error) => self.outgoing.send_error(request_id, error).await,
                }
            }
            ClientRequest::McpServerRefresh { .. }
            | ClientRequest::McpServerReconnect { .. }
            | ClientRequest::McpServerToggle { .. } => {
                let result = match self.mcp.get() {
                    Ok(control) => match client_request {
                        ClientRequest::McpServerRefresh { .. } => {
                            control.reload_mcp_servers().await
                        }
                        ClientRequest::McpServerReconnect { params, .. } => {
                            control
                                .reconnect_mcp_server(params.name.as_deref(), params.failed_only)
                                .await
                        }
                        ClientRequest::McpServerToggle { params, .. } => {
                            control
                                .toggle_mcp_server(&params.name, params.enabled)
                                .await
                        }
                        _ => unreachable!(),
                    },
                    Err(error) => Err(error),
                };
                match result {
                    Ok(()) => self.send_response(request_id, serde_json::json!({})).await,
                    Err(error) => self.outgoing.send_error(request_id, error).await,
                }
            }
            ClientRequest::Initialize { .. } => {
                // `handle_client_request` answers `initialize` before anything is dispatched.
                unreachable!("initialize is handled before initialized request dispatch")
            }
            ClientRequest::ThreadStart { params, .. } => {
                let result = self
                    .thread_processor
                    .thread_start(request_id.clone(), params, &self.thread_method_context())
                    .await;
                self.finish_turn_method(request_id, result).await;
            }
            ClientRequest::ThreadResume { params, .. } => {
                let result = self
                    .thread_processor
                    .thread_resume(request_id.clone(), params, &self.thread_method_context())
                    .await;
                self.finish_turn_method(request_id, result).await;
            }
            ClientRequest::ThreadFork { params, .. } => {
                let result = self
                    .thread_processor
                    .thread_fork(request_id.clone(), params, &self.thread_method_context())
                    .await;
                self.finish_turn_method(request_id, result).await;
            }
            ClientRequest::ThreadRead { params, .. } => {
                let result = self
                    .thread_processor
                    .thread_read(params, &self.history)
                    .await;
                self.finish_turn_method(request_id, result).await;
            }
            ClientRequest::ThreadReadStateUpdate { params, .. } => {
                let result = self
                    .thread_processor
                    .thread_read_state_update(params, &self.history)
                    .await;
                self.finish_turn_method(request_id, result).await;
            }
            ClientRequest::ThreadLoadedList { params, .. } => {
                let result = self.thread_processor.thread_loaded_list(&params);
                self.finish_turn_method(request_id, result).await;
            }
            ClientRequest::ThreadArchive { params, .. } => {
                let result = self
                    .thread_processor
                    .thread_archive(request_id.clone(), params, &self.history)
                    .await;
                self.finish_turn_method(request_id, result).await;
            }
            ClientRequest::ThreadDelete { params, .. } => {
                let result = self
                    .thread_processor
                    .thread_delete(request_id.clone(), params, &self.history)
                    .await;
                self.finish_turn_method(request_id, result).await;
            }
            ClientRequest::ThreadUnarchive { params, .. } => {
                let result = self
                    .thread_processor
                    .thread_unarchive(request_id.clone(), params, &self.history)
                    .await;
                self.finish_turn_method(request_id, result).await;
            }
            ClientRequest::ThreadSetName { params, .. } => {
                let result = self
                    .thread_processor
                    .thread_set_name(request_id.clone(), params, &self.history)
                    .await;
                self.finish_turn_method(request_id, result).await;
            }
            ClientRequest::ThreadMetadataUpdate { params, .. } => {
                let result = self
                    .thread_processor
                    .thread_metadata_update(params, &self.history)
                    .await;
                self.finish_turn_method(request_id, result).await;
            }
            ClientRequest::ThreadList { params, .. } => {
                let result = self
                    .thread_processor
                    .thread_list(params, &self.history)
                    .await;
                self.finish_turn_method(request_id, result).await;
            }
            ClientRequest::ThreadUnsubscribe { params, .. } => {
                let result = self
                    .thread_processor
                    .thread_unsubscribe(&request_id, &params)
                    .await;
                self.finish_turn_method(request_id, result).await;
            }
            ClientRequest::ThreadTurnsList { params, .. } => {
                let result = self
                    .thread_processor
                    .thread_turns_list(params, &self.history)
                    .await;
                self.finish_turn_method(request_id, result).await;
            }
            ClientRequest::ThreadItemsList { params, .. } => {
                let result = ThreadRequestProcessor::thread_items_list(&params);
                self.finish_turn_method(request_id, result).await;
            }
            ClientRequest::ThreadBackgroundTerminalsClean { params, .. } => {
                let result = self
                    .thread_processor
                    .thread_background_terminals_clean(&params)
                    .await;
                self.finish_turn_method(request_id, result).await;
            }
            ClientRequest::ThreadBackgroundTerminalsList { params, .. } => {
                let result = self
                    .thread_processor
                    .thread_background_terminals_list(params)
                    .await;
                self.finish_turn_method(request_id, result).await;
            }
            ClientRequest::ThreadBackgroundTerminalsTerminate { params, .. } => {
                let result = self
                    .thread_processor
                    .thread_background_terminals_terminate(&params)
                    .await;
                self.finish_turn_method(request_id, result).await;
            }
            ClientRequest::ThreadSettingsUpdate { params, .. } => {
                let result = self.turn_processor.thread_settings_update(params).await;
                self.finish_turn_method(request_id, result).await;
            }
            ClientRequest::TurnSettingsUpdate { params, .. } => {
                let result = self.turn_processor.turn_settings_update(params).await;
                self.finish_turn_method(request_id, result).await;
            }
            ClientRequest::TurnStart { params, .. } => {
                let result = self.turn_processor.turn_start(params).await;
                self.finish_turn_method(request_id, result).await;
            }
            ClientRequest::TurnSteer { params, .. } => {
                let result = self.turn_processor.turn_steer(params);
                self.finish_turn_method(request_id, result).await;
            }
            ClientRequest::TurnInterrupt { params, .. } => {
                let result = self
                    .turn_processor
                    .turn_interrupt(&request_id, params)
                    .await;
                self.finish_turn_method(request_id, result).await;
            }
            ClientRequest::MockExperimentalMethod { params, .. } => {
                let response = MockExperimentalMethodResponse::new(params.into_value());
                self.send_response(request_id, response).await;
            }
        }
    }

    /// Sends a method's answer, if it has one yet.
    async fn finish_turn_method(&self, request_id: ConnectionRequestId, result: TurnMethodResult) {
        match result {
            Ok(Some(result)) => self.outgoing.send_response(request_id, result).await,
            Ok(None) => {}
            Err(error) => self.outgoing.send_error(request_id, error).await,
        }
    }

    async fn send_response(&self, request_id: ConnectionRequestId, response: impl Serialize) {
        // The ported responses are plain records, so writing one does not fail; Codex answers the
        // same failure with an internal error.
        self.outgoing
            .send_serialized_response(request_id, response)
            .await;
    }
}

/// Reads `request` as one of the server's methods: Codex's `deserialize_client_request`. An
/// unknown method, or parameters that do not fit, are an invalid request.
fn deserialize_client_request(
    request: JsonRpcRequest,
) -> Result<ClientRequest, JsonRpcErrorObject> {
    ClientRequest::try_from(request)
        .map_err(|error| JsonRpcErrorObject::invalid_request(format!("Invalid request: {error}")))
}
