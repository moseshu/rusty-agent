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
//! Codex's `initialize` also does its account and product work: it selects how its gateway logs
//! in, makes the client's name the `originator` and `User-Agent` suffix of every request to its
//! backend, enables user verification for its own desktop and terminal clients, reports the
//! connection to its analytics, and registers its Windows desktop install. None of this is ported;
//! the agent's own `User-Agent` stays as it is whoever connects.

use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{Arc, OnceLock},
};

use serde::Serialize;

use super::{
    outgoing::{ConnectionRequestId, OutgoingMessageSender},
    protocol::{
        ClientRequest, InitializeParams, InitializeResponse, MockExperimentalMethodResponse,
        experimental_required_message,
    },
};
use crate::{
    frame::{JsonRpcErrorObject, JsonRpcRequest},
    transport::{ConnectionId, ConnectionOrigin},
};

/// The `User-Agent` the agent identifies itself with: the one its model adapters send.
const USER_AGENT: &str = concat!("rusty-agent/", env!("CARGO_PKG_VERSION"));

/// What the server knows of one connection: Codex's `ConnectionSessionState`.
#[derive(Debug)]
pub(crate) struct ConnectionSessionState {
    pub(crate) origin: ConnectionOrigin,
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
    pub(crate) const fn new(origin: ConnectionOrigin) -> Self {
        Self {
            origin,
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

/// Handles the requests of every connection: Codex's `MessageProcessor`, with the methods ported
/// so far.
#[derive(Debug)]
pub(crate) struct MessageProcessor {
    outgoing: Arc<OutgoingMessageSender>,
    codex_home: PathBuf,
}

impl MessageProcessor {
    pub(crate) const fn new(outgoing: Arc<OutgoingMessageSender>, codex_home: PathBuf) -> Self {
        Self {
            outgoing,
            codex_home,
        }
    }

    pub(crate) fn outgoing(&self) -> &Arc<OutgoingMessageSender> {
        &self.outgoing
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
        self.dispatch_initialized_client_request(request_id, client_request, &session)
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

        self.send_response(
            request_id,
            InitializeResponse::new(USER_AGENT, self.codex_home.clone()),
        )
        .await;
        Ok(())
    }

    /// Admits a method other than `initialize` and starts it: Codex's
    /// `dispatch_initialized_client_request`.
    ///
    /// Codex also counts the turns being admitted, so a server shutting down can refuse new ones,
    /// orders the requests that touch the same state, and tracks the requests each connection has
    /// in flight, so a closing connection can wait for them; none of the methods ported so far
    /// needs any of it.
    fn dispatch_initialized_client_request(
        self: &Arc<Self>,
        request_id: ConnectionRequestId,
        client_request: ClientRequest,
        session: &ConnectionSessionState,
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

        let processor = Arc::clone(self);
        tokio::spawn(async move {
            processor
                .handle_initialized_client_request(request_id, client_request)
                .await;
        });
        Ok(())
    }

    async fn handle_initialized_client_request(
        &self,
        request_id: ConnectionRequestId,
        client_request: ClientRequest,
    ) {
        match client_request {
            ClientRequest::Initialize { .. } => {
                // `handle_client_request` answers `initialize` before anything is dispatched.
                unreachable!("initialize is handled before initialized request dispatch")
            }
            ClientRequest::MockExperimentalMethod { params, .. } => {
                let response = MockExperimentalMethodResponse::new(params.into_value());
                self.send_response(request_id, response).await;
            }
        }
    }

    async fn send_response(&self, request_id: ConnectionRequestId, response: impl Serialize) {
        match serde_json::to_value(response) {
            Ok(result) => self.outgoing.send_response(request_id, result).await,
            // The ported responses are plain records, so this does not happen; Codex answers the
            // same failure with an internal error.
            Err(error) => {
                self.outgoing
                    .send_error(
                        request_id,
                        JsonRpcErrorObject::internal_error(format!(
                            "failed to serialize response: {error}"
                        )),
                    )
                    .await;
            }
        }
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
